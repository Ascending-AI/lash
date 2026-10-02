//! Owner-scoped, effect-keyed usage facts and dispatch liabilities.
/// version_surface = "coexist"
/// version_guard(items(LASH_USAGE_FACT_PAYLOAD_DOMAIN_VERSION, usage_correction_payload_hash, usage_fact_payload_hash))
const LASH_USAGE_FACT_PAYLOAD_DOMAIN_VERSION: &str = "lash-usage-fact-payload/v4";

use crate::usage::{SessionUsageReport, UsageAttributionKey, UsageTotals};
use crate::{LlmProfileKey, RuntimeOwner, StoreError};
use lash_sansio::TokenUsage;
use lash_sansio::llm::types::LlmCallId;
use serde::{Deserialize, Serialize};

/// The journal identity of a spending effect: `EffectAddress::graph_key()`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UsageEffectKey(String);
impl UsageEffectKey {
    pub fn for_effect(address: &lash_sansio::EffectAddress) -> Self {
        Self(address.graph_key())
    }
    /// The key the facts of `run` are settled under when `run` is a run of
    /// `effect` that dispatched and was never journaled (FIG-4632): the effect
    /// and the run. A fact's identity names its effect and not its run, so
    /// under the effect's own key these facts would collide with the facts
    /// of the run the effect is later recorded with.
    pub fn for_unrecorded_run(effect: &Self, run: &UsageRunId) -> Self {
        Self(format!("{}#{}", effect.0, run.as_str()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One execution of a spending effect's body: `run:` + 32 lowercase hex of a v4 UUID.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UsageRunId(String);
impl UsageRunId {
    pub fn mint() -> Self {
        Self(format!("run:{}", uuid::Uuid::new_v4().simple()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<UsageRunId> for String {
    fn from(run: UsageRunId) -> Self {
        run.0
    }
}
impl TryFrom<String> for UsageRunId {
    type Error = StoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.strip_prefix("run:").is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && uuid::Uuid::parse_str(id).is_ok_and(|id| {
                    id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122
                })
        });
        if valid {
            Ok(Self(value))
        } else {
            Err(corrupt(format!("invalid usage run id {value:?}")))
        }
    }
}

/// Written by a run before its first provider attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRunAdmission {
    pub owner: RuntimeOwner,
    pub effect: UsageEffectKey,
    /// `EffectJournalIdentity::key()` of the effect's execution scope.
    pub execution_scope_key: String,
    pub run: UsageRunId,
    /// Attribution of the first dispatch; unknown liabilities report under it.
    pub source: String,
    /// The recorded model key the first dispatch ran under.
    pub profile_key: LlmProfileKey,
    /// The wire model the first dispatch's request named.
    pub requested_model: String,
    pub admitted_at_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageRunAdmitted {
    Admitted,
    AlreadyAdmitted,
}
#[derive(Debug, thiserror::Error)]
pub enum UsageAdmissionError {
    /// The owner was drained (deleted or pruned); nothing may spend under it.
    #[error("usage owner {owner} retired at {retired_at_ms}")]
    OwnerRetired {
        owner: RuntimeOwner,
        retired_at_ms: u64,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageFactKind {
    Attempt,
    Correction,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UsageFactIdentity {
    pub owner: RuntimeOwner,
    pub effect: UsageEffectKey,
    /// Position of the provider call within the recorded run, 0-based, in
    /// the order the run recorded its calls. `LlmCall` and `Direct` runs have
    /// one call (0); a `ToolAttempt` run has one per nested call.
    pub call_ordinal: u32,
    /// `AttemptRecord::ordinal` within that call.
    pub provider_attempt: u32,
    pub kind: UsageFactKind,
}

/// One provider attempt of the recorded run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageAttemptFact {
    pub call_ordinal: u32,
    pub provider_attempt: u32,
    pub llm_call_id: LlmCallId,
    pub source: String,
    /// The recorded model key the call ran under: the host's registration,
    /// which two keys that share a wire model keep apart.
    pub profile_key: LlmProfileKey,
    /// The wire model the request named.
    pub requested_model: String,
    /// The model the provider reported it served this attempt with. `None`
    /// when the provider reported none: it is never filled from the request.
    pub served_model: Option<String>,
    pub outcome: AttemptFactOutcome,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptFactOutcome {
    /// `AttemptUsageOutcome::Reported`: provider usage observed, zero included
    /// (ADR 0032: `Some(0)` is a fact).
    Reported {
        usage: TokenUsage,
        generation_id: Option<String>,
    },
    /// `UnreportedAfterAbort` / `UnreportedAfterFailure`: billed, count unknown.
    Unreported { generation_id: Option<String> },
}
// An attempt whose outcome is `UnreportedByProvider` records no fact (ADR 0031, unchanged).

/// A host-invoked correction of one unreported attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCorrection {
    pub effect: UsageEffectKey,
    pub call_ordinal: u32,
    pub provider_attempt: u32,
    pub usage: TokenUsage,
    pub generation_id: String,
}

/// What the recorded run delivers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSettlement {
    pub owner: RuntimeOwner,
    pub effect: UsageEffectKey,
    pub run: UsageRunId,
    pub facts: Vec<UsageAttemptFact>,
    pub accounting: RunAccounting,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunAccounting {
    /// Every call the run began has its sealed record in `facts`.
    Complete,
    /// `calls` calls began (admission passed) and ended without a sealed
    /// record (dropped by cancellation before the provider handle returned).
    CallWithoutRecord { calls: u32 },
    /// The facts could not be journaled beside a poison entry (§7).
    FactsUnjournalable { dropped_facts: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageSettleReceipt {
    pub inserted_facts: u32,
    pub duplicate_facts: u32,
    pub run: UsageRunResolution,
    /// Other open runs of the same effect resolved `unknown(superseded_run)`.
    pub superseded_runs: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageRunResolution {
    Settled,
    Unknown(UsageUnknownReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnknownReason {
    SupersededRun,      // another run of the effect was journaled
    CallWithoutRecord,  // RunAccounting::CallWithoutRecord
    FactsUnjournalable, // RunAccounting::FactsUnjournalable
    ExecutionEnded,     // its execution was killed or lost before settling
    OwnerRetired,       // open when the owner was drained
}

/// The typed conflict: one identity, two payloads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFactConflict {
    pub identity: UsageFactIdentity,
    pub stored_payload_hash: String,
    pub offered_payload_hash: String,
}
#[derive(Debug, thiserror::Error)]
pub enum UsageAppendError {
    #[error("usage fact {0:?} conflicts with the stored fact")]
    Conflict(Box<UsageFactConflict>),
    /// A correction whose attempt fact is absent.
    #[error("correction target {identity:?} has no attempt fact")]
    CorrectionTargetMissing { identity: UsageFactIdentity },
    /// A correction of an attempt that was reported, not unreported.
    #[error("correction target {identity:?} was reported")]
    CorrectionTargetReported { identity: UsageFactIdentity },
    #[error(transparent)]
    Store(#[from] StoreError),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAppendReceipt {
    pub inserted: u32,
    pub duplicates: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageOwnerRetired {
    pub retired_at_ms: u64,
    /// Open runs this retirement resolved `unknown(owner_retired)`.
    pub resolved_open_runs: u64,
    pub already_retired: bool,
}

/// How far the owner's usage can be trusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCompleteness {
    /// Admitted runs no settlement or retirement has resolved yet: delivery pending.
    pub open_runs: u64,
    pub oldest_open_admitted_at_ms: Option<u64>,
    /// Runs that dispatched and whose amount will never be known.
    pub unknown_runs: u64,
    pub conflicted_runs: u64,
    /// Unreported attempt facts no correction has filled.
    pub unreported_attempts: u64,
    pub retired: bool,
}
impl UsageCompleteness {
    /// No open, unknown, conflicted or unreported item.
    pub fn is_complete(&self) -> bool {
        self.is_settled()
            && self.unknown_runs == 0
            && self.conflicted_runs == 0
            && self.unreported_attempts == 0
    }
    /// No open run: everything that will ever be delivered has been.
    pub fn is_settled(&self) -> bool {
        self.open_runs == 0
    }
}

/// One `(source, profile_key, requested_model)` aggregate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerUsageRow {
    pub source: String,
    pub profile_key: LlmProfileKey,
    pub requested_model: String,
    /// Reported plus reconciled counters.
    pub usage: TokenUsage,
    pub reported_attempts: u64,
    pub unreported_attempts: u64,
    pub reconciled_attempts: u64,
}
/// An unreported attempt no correction has filled, sorted by identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutstandingUsageAttempt {
    pub effect: UsageEffectKey,
    pub call_ordinal: u32,
    pub provider_attempt: u32,
    pub llm_call_id: LlmCallId,
    pub source: String,
    pub profile_key: LlmProfileKey,
    pub requested_model: String,
    pub generation_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerUsage {
    pub owner: RuntimeOwner,
    pub rows: Vec<OwnerUsageRow>, // sorted, unique by (source, profile_key, requested_model)
    /// Unreported attempts no correction covers, oldest first: the order
    /// their facts were recorded, so reconciliation works through them in
    /// the order they were spent.
    pub outstanding: Vec<OutstandingUsageAttempt>,
    pub completeness: UsageCompleteness,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageFactRecord {
    pub seq: u64,
    pub owner: RuntimeOwner,
    pub effect: UsageEffectKey,
    pub call_ordinal: u32,
    pub provider_attempt: u32,
    pub llm_call_id: LlmCallId,
    pub source: String,
    pub profile_key: LlmProfileKey,
    pub requested_model: String,
    /// Provider-reported only; a correction keeps its attempt's value.
    pub served_model: Option<String>,
    pub body: UsageFactBody,
    pub recorded_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageFactBody {
    Attempt {
        run: UsageRunId,
        outcome: AttemptFactOutcome,
    },
    Correction {
        usage: TokenUsage,
        generation_id: String,
    },
}
impl UsageFactRecord {
    pub fn identity(&self) -> UsageFactIdentity {
        UsageFactIdentity {
            owner: self.owner.clone(),
            effect: self.effect.clone(),
            call_ordinal: self.call_ordinal,
            provider_attempt: self.provider_attempt,
            kind: self.body.kind(),
        }
    }
    pub fn disposition(&self) -> UsageReporting {
        self.body.disposition()
    }
    pub fn run(&self) -> Option<&UsageRunId> {
        self.body.run()
    }
    pub fn usage(&self) -> TokenUsage {
        self.body.usage()
    }
    pub fn generation_id(&self) -> Option<&str> {
        self.body.generation_id()
    }
}
impl UsageFactBody {
    pub fn kind(&self) -> UsageFactKind {
        match self {
            Self::Attempt { .. } => UsageFactKind::Attempt,
            Self::Correction { .. } => UsageFactKind::Correction,
        }
    }
    pub fn disposition(&self) -> UsageReporting {
        match self {
            Self::Attempt {
                outcome: AttemptFactOutcome::Reported { .. },
                ..
            } => UsageReporting::Reported,
            Self::Attempt {
                outcome: AttemptFactOutcome::Unreported { .. },
                ..
            } => UsageReporting::Unreported,
            Self::Correction { .. } => UsageReporting::Reconciled,
        }
    }
    pub fn run(&self) -> Option<&UsageRunId> {
        match self {
            Self::Attempt { run, .. } => Some(run),
            Self::Correction { .. } => None,
        }
    }
    pub fn usage(&self) -> TokenUsage {
        match self {
            Self::Attempt {
                outcome: AttemptFactOutcome::Reported { usage, .. },
                ..
            }
            | Self::Correction { usage, .. } => usage.clone(),
            Self::Attempt {
                outcome: AttemptFactOutcome::Unreported { .. },
                ..
            } => TokenUsage::default(),
        }
    }
    pub fn generation_id(&self) -> Option<&str> {
        match self {
            Self::Attempt {
                outcome:
                    AttemptFactOutcome::Reported { generation_id, .. }
                    | AttemptFactOutcome::Unreported { generation_id },
                ..
            } => generation_id.as_deref(),
            Self::Correction { generation_id, .. } => Some(generation_id),
        }
    }
    /// Decode the SQL projection, rejecting combinations no body can represent.
    pub fn from_stored(
        kind: &str,
        disposition: &str,
        run: Option<UsageRunId>,
        usage: TokenUsage,
        generation_id: Option<String>,
    ) -> Result<Self, StoreError> {
        match (kind, disposition, run, generation_id) {
            ("attempt", "reported", Some(run), generation_id) => Ok(Self::Attempt {
                run,
                outcome: AttemptFactOutcome::Reported {
                    usage,
                    generation_id,
                },
            }),
            ("attempt", "unreported", Some(run), generation_id)
                if usage == TokenUsage::default() =>
            {
                Ok(Self::Attempt {
                    run,
                    outcome: AttemptFactOutcome::Unreported { generation_id },
                })
            }
            ("correction", "reconciled", None, Some(generation_id)) => Ok(Self::Correction {
                usage,
                generation_id,
            }),
            _ => Err(corrupt("invalid usage fact body")),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageReporting {
    Reported,
    Unreported,
    Reconciled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFactCursor {
    owner: RuntimeOwner,
    after_seq: u64,
}
impl UsageFactCursor {
    pub fn new(owner: RuntimeOwner, after_seq: u64) -> Self {
        Self { owner, after_seq }
    }
    pub fn after_seq(&self) -> u64 {
        self.after_seq
    }
    pub fn check_owner(&self, owner: &RuntimeOwner) -> Result<(), StoreError> {
        if &self.owner == owner {
            Ok(())
        } else {
            Err(corrupt("usage fact cursor belongs to another owner"))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFactPage {
    pub facts: Vec<UsageFactRecord>,
    pub next: Option<UsageFactCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRunRecord {
    pub effect: UsageEffectKey,
    pub run: UsageRunId,
    pub admission: Option<UsageRunDispatch>,
    pub state: UsageRunState,
}
/// Dispatch evidence recorded by admission, never inferred from a settlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRunDispatch {
    pub execution_scope_key: String,
    pub source: String,
    pub profile_key: LlmProfileKey,
    pub requested_model: String,
    pub admitted_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum UsageRunState {
    Open,
    Resolved {
        at_ms: u64,
        outcome: UsageRunOutcome,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum UsageRunOutcome {
    Settled,
    Unknown(UsageUnknownReason),
    Conflicted(UsageFactConflict),
}
impl UsageRunState {
    pub fn outcome(&self) -> Option<&UsageRunOutcome> {
        match self {
            Self::Open => None,
            Self::Resolved { outcome, .. } => Some(outcome),
        }
    }
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Resolved {
                outcome: UsageRunOutcome::Settled,
                ..
            }
        )
    }
    pub fn from_stored(
        state: &str,
        reason: Option<&str>,
        conflict: Option<UsageFactConflict>,
        resolved_at_ms: Option<u64>,
    ) -> Result<Self, StoreError> {
        match (state, reason, conflict, resolved_at_ms) {
            ("open", None, None, None) => Ok(Self::Open),
            ("settled", None, None, Some(at_ms)) => Ok(Self::Resolved {
                at_ms,
                outcome: UsageRunOutcome::Settled,
            }),
            ("unknown", Some(reason), None, Some(at_ms)) => Ok(Self::Resolved {
                at_ms,
                outcome: UsageRunOutcome::Unknown(UsageUnknownReason::from_stored(reason)?),
            }),
            ("conflicted", None, Some(conflict), Some(at_ms)) => Ok(Self::Resolved {
                at_ms,
                outcome: UsageRunOutcome::Conflicted(conflict),
            }),
            _ => Err(corrupt("invalid usage run resolution")),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageRunFilter {
    Open,
    Unresolved, /* unknown + conflicted */
    All,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRunCursor {
    owner: RuntimeOwner,
    after_effect: UsageEffectKey,
    after_run: UsageRunId,
}
impl UsageRunCursor {
    pub fn new(owner: RuntimeOwner, after_effect: UsageEffectKey, after_run: UsageRunId) -> Self {
        Self {
            owner,
            after_effect,
            after_run,
        }
    }
    pub fn after_effect(&self) -> &UsageEffectKey {
        &self.after_effect
    }
    pub fn after_run(&self) -> &UsageRunId {
        &self.after_run
    }
    pub fn check_owner(&self, owner: &RuntimeOwner) -> Result<(), StoreError> {
        if &self.owner == owner {
            Ok(())
        } else {
            Err(corrupt("usage run cursor belongs to another owner"))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRunPage {
    pub runs: Vec<UsageRunRecord>,
    pub next: Option<UsageRunCursor>,
}

/// Payload identity (moved here from `store/runtime_commit.rs`; same constant,
/// same value under the version freeze, new projection in place).
///
/// version_guard(
///     roots(PayloadAttribution, UsageAttemptFact, UsageCorrection, UsageFactRecord),
///     items(
///         usage_fact_payload_hash, usage_correction_payload_hash, attempt_payload_bytes,
///         correction_payload_bytes, payload_bytes, as_str,
///     ),
/// )
pub const USAGE_PAYLOAD_FAMILY_VERSION: u8 = 4;
/// BLAKE3 hex under domain `lash-usage-fact-payload/v4` of the framed
/// projection: kind, disposition tag, source, model key, requested model,
/// optional served model, the five counters (big-endian i64), llm_call_id,
/// optional generation_id, optional run id.
/// The identity columns are not part of the payload. Full destructures, no `..`.
pub fn usage_fact_payload_hash(fact: &UsageAttemptFact, run: &UsageRunId) -> String {
    crate::stable_hash::blake3_hex(
        LASH_USAGE_FACT_PAYLOAD_DOMAIN_VERSION,
        &attempt_payload_bytes(fact, run),
    )
}

/// The payload hash of `correction` against `target`, the unreported attempt
/// it fills: the correction keeps that attempt's call and attribution.
pub fn usage_correction_payload_hash(
    correction: &UsageCorrection,
    target: &UsageFactRecord,
) -> String {
    crate::stable_hash::blake3_hex(
        LASH_USAGE_FACT_PAYLOAD_DOMAIN_VERSION,
        &correction_payload_bytes(correction, target),
    )
}

fn attempt_payload_bytes(fact: &UsageAttemptFact, run: &UsageRunId) -> Vec<u8> {
    let UsageAttemptFact {
        call_ordinal: _,
        provider_attempt: _,
        llm_call_id,
        source,
        profile_key,
        requested_model,
        served_model,
        outcome,
    } = fact;
    let attribution = PayloadAttribution {
        source,
        profile_key,
        requested_model,
        served_model: served_model.as_deref(),
    };
    match outcome {
        AttemptFactOutcome::Reported {
            usage,
            generation_id,
        } => payload_bytes(
            0,
            0,
            attribution,
            usage,
            llm_call_id,
            generation_id.as_deref(),
            Some(run),
        ),
        AttemptFactOutcome::Unreported { generation_id } => payload_bytes(
            0,
            1,
            attribution,
            &TokenUsage::default(),
            llm_call_id,
            generation_id.as_deref(),
            Some(run),
        ),
    }
}
fn correction_payload_bytes(correction: &UsageCorrection, target: &UsageFactRecord) -> Vec<u8> {
    let UsageCorrection {
        effect: _,
        call_ordinal: _,
        provider_attempt: _,
        usage,
        generation_id,
    } = correction;
    payload_bytes(
        1,
        2,
        PayloadAttribution {
            source: &target.source,
            profile_key: &target.profile_key,
            requested_model: &target.requested_model,
            served_model: target.served_model.as_deref(),
        },
        usage,
        &target.llm_call_id,
        Some(generation_id),
        None,
    )
}
/// What a fact's payload says the spend is attributed to.
struct PayloadAttribution<'a> {
    source: &'a str,
    profile_key: &'a LlmProfileKey,
    requested_model: &'a str,
    served_model: Option<&'a str>,
}
fn payload_bytes(
    kind: u8,
    disposition: u8,
    attribution: PayloadAttribution<'_>,
    usage: &TokenUsage,
    call: &LlmCallId,
    generation: Option<&str>,
    run: Option<&UsageRunId>,
) -> Vec<u8> {
    let TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
    } = usage;
    let mut encoder = crate::stable_identity::IdentityEncoder::new(
        "lash.runtime-usage-payload",
        USAGE_PAYLOAD_FAMILY_VERSION,
    );
    encoder.tag(kind);
    encoder.tag(disposition);
    let PayloadAttribution {
        source,
        profile_key,
        requested_model,
        served_model,
    } = attribution;
    encoder.string(source);
    encoder.string(profile_key.as_str());
    encoder.string(requested_model);
    encoder.optional(served_model, |encoder, value| encoder.string(value));
    for counter in [
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
    ] {
        encoder.i64(*counter);
    }
    encoder.string(call.0.as_str());
    encoder.optional(generation, |encoder, value| encoder.string(value));
    encoder.optional(run, |encoder, value| encoder.string(value.as_str()));
    encoder.finish()
}

fn corrupt(message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: "usage accounting",
        message: message.into(),
    }
}

/// SQL owner columns, shared by both backend encoders.
pub fn usage_owner_columns(owner: &RuntimeOwner) -> (&'static str, &str) {
    match owner {
        RuntimeOwner::Session(id) => ("session", id.as_str()),
        RuntimeOwner::Process(id) => ("process", id.as_str()),
    }
}

impl OwnerUsage {
    pub fn report(&self) -> SessionUsageReport {
        let mut report = SessionUsageReport::default();
        for row in &self.rows {
            let total_tokens = [
                row.usage.input_tokens,
                row.usage.output_tokens,
                row.usage.cache_read_input_tokens,
                row.usage.cache_write_input_tokens,
            ]
            .into_iter()
            .fold(0_i64, |total, counter| {
                total.checked_add(counter).unwrap_or_else(|| {
                    report.saturated = true;
                    total.saturating_add(counter)
                })
            });
            let totals = UsageTotals {
                usage: row.usage.clone(),
                total_tokens,
                unreported_attempts: u32::try_from(row.unreported_attempts).unwrap_or(u32::MAX),
                reconciled_attempts: u32::try_from(row.reconciled_attempts).unwrap_or(u32::MAX),
            };
            report.saturated |= row.unreported_attempts > u64::from(u32::MAX)
                || row.reconciled_attempts > u64::from(u32::MAX);
            report.entry_count = report.entry_count.saturating_add(
                usize::try_from(
                    row.reported_attempts
                        .saturating_add(row.unreported_attempts)
                        .saturating_add(row.reconciled_attempts.saturating_mul(2)),
                )
                .unwrap_or(usize::MAX),
            );
            absorb_totals(&mut report.usage, &totals, &mut report.saturated);
            absorb_totals(
                report.by_source.entry(row.source.clone()).or_default(),
                &totals,
                &mut report.saturated,
            );
            absorb_totals(
                report
                    .by_profile_key
                    .entry(row.profile_key.clone())
                    .or_default(),
                &totals,
                &mut report.saturated,
            );
            absorb_totals(
                report
                    .by_requested_model
                    .entry(row.requested_model.clone())
                    .or_default(),
                &totals,
                &mut report.saturated,
            );
            report.by_attribution.insert(
                UsageAttributionKey {
                    source: row.source.clone(),
                    profile_key: row.profile_key.clone(),
                    requested_model: row.requested_model.clone(),
                },
                totals,
            );
        }
        report
    }
}
fn absorb_totals(target: &mut UsageTotals, incoming: &UsageTotals, saturated: &mut bool) {
    macro_rules! add {
        ($field:ident) => {
            if let Some(sum) = target.usage.$field.checked_add(incoming.usage.$field) {
                target.usage.$field = sum;
            } else {
                *saturated = true;
                target.usage.$field = target.usage.$field.saturating_add(incoming.usage.$field);
            }
        };
    }
    add!(input_tokens);
    add!(output_tokens);
    add!(cache_read_input_tokens);
    add!(cache_write_input_tokens);
    add!(reasoning_output_tokens);
    if let Some(sum) = target.total_tokens.checked_add(incoming.total_tokens) {
        target.total_tokens = sum;
    } else {
        *saturated = true;
        target.total_tokens = target.total_tokens.saturating_add(incoming.total_tokens);
    }
    target.unreported_attempts = target
        .unreported_attempts
        .saturating_add(incoming.unreported_attempts);
    target.reconciled_attempts = target
        .reconciled_attempts
        .saturating_add(incoming.reconciled_attempts);
}

impl UsageAttemptFact {
    /// The row projection of this attempt. Identity columns do not enter its hash.
    pub fn record(
        &self,
        owner: &RuntimeOwner,
        effect: &UsageEffectKey,
        run: &UsageRunId,
        now_ms: u64,
    ) -> UsageFactRecord {
        let UsageAttemptFact {
            call_ordinal,
            provider_attempt,
            llm_call_id,
            source,
            profile_key,
            requested_model,
            served_model,
            outcome,
        } = self;
        UsageFactRecord {
            seq: 0,
            owner: owner.clone(),
            effect: effect.clone(),
            call_ordinal: *call_ordinal,
            provider_attempt: *provider_attempt,
            llm_call_id: llm_call_id.clone(),
            source: source.clone(),
            profile_key: profile_key.clone(),
            requested_model: requested_model.clone(),
            served_model: served_model.clone(),
            body: UsageFactBody::Attempt {
                run: run.clone(),
                outcome: outcome.clone(),
            },
            recorded_at_ms: now_ms,
        }
    }
}
impl RunAccounting {
    pub fn resolution(&self) -> UsageRunResolution {
        match self {
            Self::Complete => UsageRunResolution::Settled,
            Self::CallWithoutRecord { .. } => {
                UsageRunResolution::Unknown(UsageUnknownReason::CallWithoutRecord)
            }
            Self::FactsUnjournalable { .. } => {
                UsageRunResolution::Unknown(UsageUnknownReason::FactsUnjournalable)
            }
        }
    }
}
impl UsageUnknownReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SupersededRun => "superseded_run",
            Self::CallWithoutRecord => "call_without_record",
            Self::FactsUnjournalable => "facts_unjournalable",
            Self::ExecutionEnded => "execution_ended",
            Self::OwnerRetired => "owner_retired",
        }
    }
    pub fn from_stored(value: &str) -> Result<Self, StoreError> {
        match value {
            "superseded_run" => Ok(Self::SupersededRun),
            "call_without_record" => Ok(Self::CallWithoutRecord),
            "facts_unjournalable" => Ok(Self::FactsUnjournalable),
            "execution_ended" => Ok(Self::ExecutionEnded),
            "owner_retired" => Ok(Self::OwnerRetired),
            _ => Err(corrupt(format!("invalid unknown reason {value:?}"))),
        }
    }
}
impl UsageFactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Attempt => "attempt",
            Self::Correction => "correction",
        }
    }
}
impl UsageReporting {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Unreported => "unreported",
            Self::Reconciled => "reconciled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_fact_bodies_reject_every_inconsistent_kind_disposition_and_run() {
        for kind in ["attempt", "correction"] {
            for disposition in ["reported", "unreported", "reconciled"] {
                for run in [None, Some(UsageRunId::mint())] {
                    for generation_id in [None, Some("generation".to_owned())] {
                        let valid = matches!(
                            (kind, disposition, run.is_some(), generation_id.is_some()),
                            ("attempt", "reported" | "unreported", true, _)
                                | ("correction", "reconciled", false, true)
                        );
                        assert_eq!(
                            UsageFactBody::from_stored(
                                kind,
                                disposition,
                                run.clone(),
                                TokenUsage::default(),
                                generation_id
                            )
                            .is_ok(),
                            valid
                        );
                    }
                }
            }
        }
        assert!(
            UsageFactBody::from_stored(
                "attempt",
                "unreported",
                Some(UsageRunId::mint()),
                TokenUsage {
                    input_tokens: 1,
                    ..Default::default()
                },
                None
            )
            .is_err()
        );
    }

    #[test]
    fn run_resolution_rejects_missing_or_extraneous_state_evidence() {
        for state in ["open", "settled", "unknown", "conflicted"] {
            for reason in [None, Some("execution_ended")] {
                for resolved in [None, Some(10)] {
                    let valid = matches!(
                        (state, reason, resolved),
                        ("open", None, None)
                            | ("settled", None, Some(_))
                            | ("unknown", Some(_), Some(_))
                    );
                    assert_eq!(
                        UsageRunState::from_stored(state, reason, None, resolved).is_ok(),
                        valid
                    );
                }
            }
        }
        assert!(
            serde_json::from_value::<UsageRunState>(
                serde_json::json!({"Resolved": {"outcome": "Settled"}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<UsageFactBody>(
                serde_json::json!({"kind": "correction", "usage": TokenUsage::default()})
            )
            .is_err()
        );
    }

    fn corpus() -> Vec<(String, Vec<u8>, String)> {
        let run = UsageRunId::try_from("run:00000000000040008000000000000001".to_owned())
            .expect("run id");
        let base = UsageAttemptFact {
            call_ordinal: 0,
            provider_attempt: 0,
            llm_call_id: LlmCallId("call:é".into()),
            source: "turn".into(),
            profile_key: LlmProfileKey::new("key"),
            requested_model: "model".into(),
            served_model: Some("served".into()),
            outcome: AttemptFactOutcome::Reported {
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_read_input_tokens: 2,
                    cache_write_input_tokens: 1,
                    reasoning_output_tokens: 4,
                },
                generation_id: Some("generation".into()),
            },
        };
        let mut cases = Vec::new();
        let mut push = |name: &str, fact: UsageAttemptFact| {
            cases.push((
                name.to_owned(),
                attempt_payload_bytes(&fact, &run),
                usage_fact_payload_hash(&fact, &run),
            ));
        };
        push("reported", base.clone());
        let mut zero = base.clone();
        zero.outcome = AttemptFactOutcome::Reported {
            usage: TokenUsage::default(),
            generation_id: None,
        };
        push("reported_zero", zero);
        let mut unreported = base.clone();
        unreported.outcome = AttemptFactOutcome::Unreported {
            generation_id: Some("generation".into()),
        };
        push("unreported_generation", unreported.clone());
        unreported.outcome = AttemptFactOutcome::Unreported {
            generation_id: None,
        };
        push("unreported_no_generation", unreported);
        let mut extremes = base.clone();
        extremes.source = "".into();
        extremes.profile_key = LlmProfileKey::new("");
        extremes.requested_model = "a\0b".into();
        extremes.served_model = None;
        extremes.outcome = AttemptFactOutcome::Reported {
            usage: TokenUsage {
                input_tokens: i64::MIN,
                output_tokens: i64::MAX,
                cache_read_input_tokens: -1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            },
            generation_id: Some("".into()),
        };
        push("signed_extremes", extremes);
        let correction = UsageCorrection {
            effect: serde_json::from_str("\"effect\"").expect("effect"),
            call_ordinal: 0,
            provider_attempt: 0,
            usage: TokenUsage {
                input_tokens: 11,
                output_tokens: 5,
                ..Default::default()
            },
            generation_id: "recovered".into(),
        };
        let target = base.record(
            &RuntimeOwner::Session(lash_sansio::SessionId::from("owner")),
            &correction.effect,
            &run,
            0,
        );
        cases.push((
            "correction".into(),
            correction_payload_bytes(&correction, &target),
            usage_correction_payload_hash(&correction, &target),
        ));
        cases
    }
    #[test]
    fn usage_fact_payload_v4_golden_corpus() {
        let rendered = corpus()
            .into_iter()
            .map(|(name, bytes, hash)| {
                let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                format!("{name}={hex}|{hash}\n")
            })
            .collect::<String>();
        assert_eq!(rendered, include_str!("testdata/usage_fact_payload_v4.hex"));
    }
    #[test]
    fn run_ids_accept_only_canonical_v4_renderings() {
        assert!(UsageRunId::try_from(UsageRunId::mint().as_str().to_owned()).is_ok());
        for bad in [
            "",
            "run:00000000000070008000000000000001",
            "run:00000000000040000000000000000001",
            "run:0000000000004000800000000000000A",
            "run:00000000-0000-4000-8000-000000000001",
        ] {
            assert!(matches!(
                UsageRunId::try_from(bad.to_owned()),
                Err(StoreError::StoredDataCorrupt { .. })
            ));
        }
    }
    #[test]
    fn fact_hash_covers_payload_and_excludes_identity() {
        let run = UsageRunId::mint();
        let mut fact = UsageAttemptFact {
            call_ordinal: 0,
            provider_attempt: 0,
            llm_call_id: LlmCallId("call".into()),
            source: "turn".into(),
            profile_key: LlmProfileKey::new("key"),
            requested_model: "model".into(),
            served_model: None,
            outcome: AttemptFactOutcome::Reported {
                usage: Default::default(),
                generation_id: None,
            },
        };
        let hash = usage_fact_payload_hash(&fact, &run);
        fact.call_ordinal = 7;
        fact.provider_attempt = 9;
        assert_eq!(hash, usage_fact_payload_hash(&fact, &run));
        let mut changed = fact.clone();
        changed.llm_call_id = LlmCallId("other".into());
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        changed = fact.clone();
        changed.source.push('x');
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        changed = fact.clone();
        changed.profile_key = LlmProfileKey::new("another-key");
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        changed = fact.clone();
        changed.requested_model.push('x');
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        changed = fact.clone();
        changed.served_model = Some("model".into());
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        changed = fact.clone();
        changed.outcome = AttemptFactOutcome::Unreported {
            generation_id: None,
        };
        assert_ne!(hash, usage_fact_payload_hash(&changed, &run));
        assert_ne!(hash, usage_fact_payload_hash(&fact, &UsageRunId::mint()));
    }
}
