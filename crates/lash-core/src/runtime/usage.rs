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

/// One interrupted attempt recorded inside an [`LedgerUsageDisposition::Unreported`]
/// ledger row: the durable identity of a billed-but-uncounted call.
///
/// The row's `(source, model)` pair supplies the attribution the descriptor
/// deliberately omits, so a row can never disagree with its own holes. Identity
/// is `(session_id, call_id, attempt_ordinal)`; `generation_id` is lookup
/// attribution, not identity, and its absence is a fact (the attempt never got
/// far enough to have one) rather than missing data.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct UnreportedLedgerAttempt {
    /// The sealed call the attempt belongs to.
    pub call_id: String,
    /// The attempt within that call.
    pub attempt_ordinal: u32,
    /// Provider generation id, when the attempt got far enough to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

impl UnreportedLedgerAttempt {
    /// The durable key of this hole within its session.
    pub fn key(&self) -> (&str, u32) {
        (self.call_id.as_str(), self.attempt_ordinal)
    }
}

/// A stored disposition that does not describe a legal ledger row.
///
/// Reads are strict: a malformed persisted disposition is a typed refusal, never
/// silently downgraded to [`LedgerUsageDisposition::Reported`] — that downgrade
/// is exactly how a billed call becomes a free one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageDispositionError {
    /// An `Unreported` row carried no holes: the hole is the row's content.
    EmptyUnreportedRow,
    /// Two holes in one row claimed the same `(call_id, attempt_ordinal)`.
    DuplicateAttempt {
        /// The call whose attempt appeared twice.
        call_id: String,
        /// The attempt ordinal that appeared twice.
        attempt_ordinal: u32,
    },
    /// One `(call_id, attempt_ordinal)` was attributed to two generations.
    ConflictingAttribution {
        /// The call whose attribution disagreed.
        call_id: String,
        /// The attempt ordinal whose attribution disagreed.
        attempt_ordinal: u32,
    },
}

impl std::fmt::Display for UsageDispositionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUnreportedRow => {
                write!(f, "unreported usage row carries no interrupted attempts")
            }
            Self::DuplicateAttempt {
                call_id,
                attempt_ordinal,
            } => write!(
                f,
                "unreported usage row repeats attempt `{call_id}`/{attempt_ordinal}"
            ),
            Self::ConflictingAttribution {
                call_id,
                attempt_ordinal,
            } => write!(
                f,
                "unreported attempt `{call_id}`/{attempt_ordinal} carries conflicting generation attribution"
            ),
        }
    }
}

impl std::error::Error for UsageDispositionError {}

/// How one ledger row relates to provider-reported usage.
///
/// ADR 0031: absence means unreported and an explicit zero is information. A
/// turn whose attempt ended before the provider's usage arrived writes an
/// `Unreported` row — zero counters, one descriptor per billed-but-uncounted
/// call — so a host summing cost sees the hole *and* can reconcile it after a
/// restart. A later host-invoked reconciliation appends a `Reconciled`
/// correction row attributed to the original attempt; rows are never rewritten.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerUsageDisposition {
    /// Provider-reported usage, accumulated per `(source, model)`.
    #[default]
    Reported,
    /// Attempts that ended without provider usage. `usage` is zero; each
    /// descriptor names one billed-but-uncounted call, so the outstanding work
    /// survives a store round trip instead of collapsing to a count.
    Unreported {
        /// Interrupted attempts whose usage never arrived, in canonical
        /// `(call_id, attempt_ordinal)` order.
        attempts: Vec<UnreportedLedgerAttempt>,
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
    /// An unreported row over `attempts`, canonicalised: sorted by
    /// `(call_id, attempt_ordinal)` so a row's identity does not depend on the
    /// order holes happened to be merged in.
    pub fn unreported(attempts: impl IntoIterator<Item = UnreportedLedgerAttempt>) -> Self {
        let mut attempts = attempts.into_iter().collect::<Vec<_>>();
        canonicalize_attempts(&mut attempts);
        Self::Unreported { attempts }
    }

    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }

    /// Reject a row that cannot describe real accounting. Store reads call this
    /// before admitting a persisted disposition.
    pub fn validate(&self) -> Result<(), UsageDispositionError> {
        let Self::Unreported { attempts } = self else {
            return Ok(());
        };
        if attempts.is_empty() {
            return Err(UsageDispositionError::EmptyUnreportedRow);
        }
        for (index, attempt) in attempts.iter().enumerate() {
            if let Some(previous) = attempts[..index]
                .iter()
                .find(|previous| previous.key() == attempt.key())
            {
                return Err(if previous.generation_id == attempt.generation_id {
                    UsageDispositionError::DuplicateAttempt {
                        call_id: attempt.call_id.clone(),
                        attempt_ordinal: attempt.attempt_ordinal,
                    }
                } else {
                    UsageDispositionError::ConflictingAttribution {
                        call_id: attempt.call_id.clone(),
                        attempt_ordinal: attempt.attempt_ordinal,
                    }
                });
            }
        }
        Ok(())
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
            Self::Unreported { attempts } => u32::try_from(attempts.len()).unwrap_or(u32::MAX),
            Self::Reported | Self::Reconciled { .. } => 0,
        }
    }

    /// The holes this row carries, empty for every other disposition.
    pub fn unreported_attempt_descriptors(&self) -> &[UnreportedLedgerAttempt] {
        match self {
            Self::Unreported { attempts } => attempts.as_slice(),
            Self::Reported | Self::Reconciled { .. } => &[],
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

    /// Fold `other` into `self` for an accumulating pair. Holes are merged by
    /// identity: a repeat of a hole already held is idempotent, and a
    /// disagreeing generation attribution is refused. Returns the conflict for
    /// the checked path; the saturating path keeps the row it already holds.
    pub(crate) fn absorb_saturating(&mut self, other: &Self) -> Option<UsageDispositionError> {
        let (Self::Unreported { attempts }, Self::Unreported { attempts: incoming }) =
            (self, other)
        else {
            return None;
        };
        let mut conflict = None;
        for candidate in incoming {
            match attempts
                .iter()
                .find(|existing| existing.key() == candidate.key())
            {
                Some(existing) => {
                    if existing.generation_id != candidate.generation_id && conflict.is_none() {
                        conflict = Some(UsageDispositionError::ConflictingAttribution {
                            call_id: candidate.call_id.clone(),
                            attempt_ordinal: candidate.attempt_ordinal,
                        });
                    }
                }
                None => attempts.push(candidate.clone()),
            }
        }
        canonicalize_attempts(attempts);
        conflict
    }
}

fn canonicalize_attempts(attempts: &mut [UnreportedLedgerAttempt]) {
    attempts.sort_by(|left, right| left.key().cmp(&right.key()));
}

/// Rebuild the attempts a session still owes usage for from its ledger rows:
/// every hole any row carries, minus every hole a `Reconciled` correction has
/// already filled. Order-independent — corrections may precede their holes in
/// the durable row order — and the sole way a reopened runtime learns what is
/// outstanding.
pub fn outstanding_unreported_attempts(
    entries: &[TokenLedgerEntry],
) -> Vec<UnreportedUsageAttempt> {
    let mut outstanding = Vec::<UnreportedUsageAttempt>::new();
    let mut reconciled = std::collections::HashSet::<(String, u32)>::new();
    for entry in entries {
        if let LedgerUsageDisposition::Reconciled {
            call_id,
            attempt_ordinal,
        } = &entry.usage_disposition
        {
            reconciled.insert((call_id.clone(), *attempt_ordinal));
        }
        for attempt in entry.usage_disposition.unreported_attempt_descriptors() {
            let key = (attempt.call_id.clone(), attempt.attempt_ordinal);
            if outstanding
                .iter()
                .any(|held| (held.call_id.clone(), held.attempt_ordinal) == key)
            {
                continue;
            }
            outstanding.push(UnreportedUsageAttempt {
                call_id: attempt.call_id.clone(),
                attempt_ordinal: attempt.attempt_ordinal,
                source: entry.source.clone(),
                model: entry.model.clone(),
                generation_id: attempt.generation_id.clone(),
            });
        }
    }
    outstanding.retain(|attempt| {
        !reconciled.contains(&(attempt.call_id.clone(), attempt.attempt_ordinal))
    });
    outstanding
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
        // A disagreeing hole attribution cannot be resolved on this infallible
        // path: keep the attribution already held and mark the report inexact.
        // The checked commit path refuses the same conflict outright.
        existing
            .usage_disposition
            .absorb_saturating(&entry.usage_disposition)
            .is_some()
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
