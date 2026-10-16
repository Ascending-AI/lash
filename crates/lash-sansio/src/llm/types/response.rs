//! Response outcomes and provider execution evidence.

use super::*;

/// Text observed after the first complete cell, excluded from execution and history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardedAfterCell {
    /// Number of observed UTF-8 bytes after the cell boundary.
    pub bytes: usize,
    /// Why the text was excluded.
    pub reason: DiscardReason,
}

/// Why a protocol discarded observed response text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscardReason {
    /// The model continued before receiving the cell's real results.
    PredictedContinuation,
}

/// Facts reported by the provider about the execution that produced a response.
///
/// These fields must never be filled from request intent. In particular,
/// `reasoning_output_tokens: Some(0)` is distinct from an unreported value.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
pub struct ExecutionEvidence {
    #[serde(default)]
    pub served_model: Option<String>,
    #[serde(default)]
    pub provider_response_id: Option<String>,
    /// Transport request identifier reported by the provider (for example,
    /// OpenRouter's `x-request-id`). This is distinct from the response's
    /// protocol-level identifier and may be present on failed attempts.
    #[serde(default)]
    pub provider_request_id: Option<String>,
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported terminal reason in that provider's own vocabulary.
    /// Compatible gateways prefer their native reason over a normalized
    /// OpenAI-compatible `finish_reason` when both are present.
    #[serde(default)]
    pub provider_finish_reason: Option<String>,
    /// Why provider evidence is partial even though the Lash response is
    /// accepted. Present only when Lash deliberately preempted collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_interruption: Option<ExecutionEvidenceCollectionInterruption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecutionEvidenceMergeError {
    BeforeResponseStart,
    IdentityConflict {
        field: &'static str,
        current: String,
        next: String,
    },
}

impl ExecutionEvidenceMergeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BeforeResponseStart => "stream_evidence_before_response_start",
            Self::IdentityConflict { .. } => "stream_evidence_identity_conflict",
        }
    }
}

impl std::fmt::Display for ExecutionEvidenceMergeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeResponseStart => {
                formatter.write_str("provider execution evidence preceded response establishment")
            }
            Self::IdentityConflict {
                field,
                current,
                next,
            } => write!(
                formatter,
                "provider execution evidence changed {field} from `{current}` to `{next}`"
            ),
        }
    }
}

impl std::error::Error for ExecutionEvidenceMergeError {}

impl ExecutionEvidence {
    pub fn merge_optional(
        accumulated: &mut Option<Self>,
        next: Option<Self>,
    ) -> Result<(), ExecutionEvidenceMergeError> {
        let Some(next) = next else { return Ok(()) };
        if next == Self::default() {
            return Ok(());
        }
        let mut merged = accumulated.clone().unwrap_or_default();
        merged.merge(next)?;
        *accumulated = Some(merged);
        Ok(())
    }

    pub fn merge(&mut self, next: Self) -> Result<(), ExecutionEvidenceMergeError> {
        fn merge_identity(
            current: &mut Option<String>,
            next: Option<String>,
            field: &'static str,
        ) -> Result<(), ExecutionEvidenceMergeError> {
            match (current.as_deref(), next) {
                (Some(existing), Some(next)) if existing != next => {
                    Err(ExecutionEvidenceMergeError::IdentityConflict {
                        field,
                        current: existing.to_string(),
                        next,
                    })
                }
                (None, Some(next)) => {
                    *current = Some(next);
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        let ExecutionEvidence {
            served_model,
            provider_response_id,
            provider_request_id,
            reasoning_output_tokens,
            provider_finish_reason,
            collection_interruption,
        } = next;
        let mut merged = self.clone();
        merge_identity(&mut merged.served_model, served_model, "served_model")?;
        merge_identity(
            &mut merged.provider_response_id,
            provider_response_id,
            "provider_response_id",
        )?;
        merge_identity(
            &mut merged.provider_request_id,
            provider_request_id,
            "provider_request_id",
        )?;
        merged.reasoning_output_tokens =
            match (merged.reasoning_output_tokens, reasoning_output_tokens) {
                (Some(current), Some(next)) => Some(current.max(next)),
                (current, next) => current.or(next),
            };
        merged.provider_finish_reason = provider_finish_reason.or(merged.provider_finish_reason);
        merged.collection_interruption = collection_interruption.or(merged.collection_interruption);
        *self = merged;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEvidenceCollectionInterruption {
    ProtocolAbort,
}

/// Lash-owned identity for one logical LLM call, spanning all transport
/// attempts made by the retry owner.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct LlmCallId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    Aborted,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolPosition {
    NoResponse,
    ResponseObserved,
    OutputStarted,
    TerminalObserved,
}

/// A journal-safe projection of a provider/transport failure.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct NormalizedError {
    pub class: ProviderFailureKind,
    /// The failure code, namespaced by who authored the spelling: `lash:` for
    /// workspace-authored codes, `provider:` for provider wire vocabulary,
    /// and a host's own namespace for host-authored codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::session_model::FailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<std::time::Duration>,
}

/// Why host charge-safety policy denied an otherwise transport-retryable
/// generation.
///
/// # Integrator class
///
/// Runtime and reporting implementors exhaustively render these host-policy
/// outcomes; provider adapters never construct them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChargeSafetyDenialReason {
    /// Host policy requires a provider idempotency or resume guarantee.
    GuaranteeRequired,
    /// The per-call unsafe retry allowance is exhausted.
    UnsafeRetryLimitExceeded,
    /// Provider-reported tokens exceed the host's duplicate-cost bound.
    DuplicateCostLimitExceeded,
}

impl ChargeSafetyDenialReason {
    /// The terminal provider code for these refusal facts.
    pub const fn failure_code(
        self,
        position: ProtocolPosition,
    ) -> crate::session_model::TurnFailureCode {
        use crate::session_model::TurnFailureCode;
        match self {
            Self::GuaranteeRequired => match position {
                ProtocolPosition::NoResponse => {
                    TurnFailureCode::UnsafeRetryWithoutTransportClassification
                }
                ProtocolPosition::ResponseObserved => {
                    TurnFailureCode::UnsafeRetryAfterResponseObserved
                }
                ProtocolPosition::OutputStarted => TurnFailureCode::UnsafeRetryAfterOutputStarted,
                ProtocolPosition::TerminalObserved => {
                    TurnFailureCode::UnsafeRetryAfterTerminalObserved
                }
            },
            Self::UnsafeRetryLimitExceeded => TurnFailureCode::ChargeSafetyUnsafeRetryLimitExceeded,
            Self::DuplicateCostLimitExceeded => {
                TurnFailureCode::ChargeSafetyDuplicateCostLimitExceeded
            }
        }
    }
}

/// Typed outcome of consulting host charge-safety policy for one prospective
/// unsafe retry.
///
/// # Integrator class
///
/// Runtime and reporting implementors consume this evidence. Host
/// applications configure `ChargeSafetyPolicy` instead of constructing an
/// outcome directly.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ChargeSafetyDecision {
    /// Host policy permits this otherwise unsafe retry.
    Authorized {
        /// Provider-reported tokens billed for the abandoned generation.
        tokens_at_stake: u64,
        /// One-based unsafe retry number within this logical LLM call.
        attempt_number: u8,
    },
    /// Host policy refuses this otherwise unsafe retry.
    Denied {
        /// Provider-reported tokens billed for the abandoned generation.
        tokens_at_stake: u64,
        /// One-based unsafe retry number within this logical LLM call.
        attempt_number: u8,
        /// Typed policy bound that refused the retry.
        reason: ChargeSafetyDenialReason,
    },
}

/// ADR 0031: absence means unreported and an explicit zero is information.
/// The outcome derives the reason for absent usage from the sealed attempt,
/// so a host summing cost can tell "the provider reported nothing" from "the
/// call was cut off before the provider's final usage chunk arrived" — the
/// latter is billed by the provider even though lash never saw the count.
///
/// A `Reported` outcome means provider usage was observed before the
/// attempt ended. Dialects that stream usage incrementally may have delivered
/// only an early partial; the record carries what was observed, never a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptUsageOutcome {
    /// Provider-reported usage was observed; `usage` is `Some`.
    #[default]
    Reported,
    /// The attempt completed and the provider reported no usage at all.
    UnreportedByProvider,
    /// The attempt was aborted — a protocol-owned stream abort or an explicit
    /// cancellation — before the provider's usage arrived. The provider may
    /// still bill the generation; a host may reconcile its provider receipts.
    UnreportedAfterAbort,
    /// The attempt failed or was interrupted before the provider's usage
    /// arrived.
    UnreportedAfterFailure,
}

impl AttemptUsageOutcome {
    /// Derive the outcome from what the attempt actually observed.
    pub fn for_attempt(outcome: AttemptOutcome, usage: Option<&LlmUsage>) -> Self {
        if usage.is_some() {
            return Self::Reported;
        }
        match outcome {
            AttemptOutcome::Completed => Self::UnreportedByProvider,
            AttemptOutcome::Aborted => Self::UnreportedAfterAbort,
            AttemptOutcome::Failed | AttemptOutcome::Interrupted => Self::UnreportedAfterFailure,
        }
    }

    /// True when the attempt ended early and its provider usage never arrived
    /// — the hole a usage ledger must show rather than silently sum as zero.
    pub fn is_unreported_after_interruption(self) -> bool {
        matches!(
            self,
            Self::UnreportedAfterAbort | Self::UnreportedAfterFailure
        )
    }

    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct AttemptRecord {
    pub ordinal: u32,
    pub outcome: AttemptOutcome,
    pub protocol_position: ProtocolPosition,
    pub retry_budget_consumed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_decision: Option<RetryDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<NormalizedError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ExecutionEvidence>,
    /// Which of the caller's generation options this attempt's request
    /// carried, as reported by the adapter that built it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<GenerationReceipt>,
    /// Provider-reported usage only. Absence is not zero usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
}

impl AttemptRecord {
    /// Why provider usage is present or absent for this outcome.
    pub fn usage_disposition(&self) -> AttemptUsageOutcome {
        AttemptUsageOutcome::for_attempt(self.outcome, self.usage.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmCallRecord {
    pub call_id: LlmCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Semantic replay metadata removed before this call reached the wire.
    ///
    /// This is part of the sealed call record so evidence survives when
    /// provider-payload tracing is disabled and across durable runtime-effect
    /// boundaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_drops: Vec<ProviderReplayDrop>,
    pub attempts: Vec<AttemptRecord>,
}

impl LlmCallRecord {
    /// Whether the call's usage is unknown: the attempt that ended it was
    /// cut short after a response was observed, before the provider's usage
    /// arrived. The provider may bill it, so a usage sum counts it unknown,
    /// never as zero (ADR 0031).
    pub fn usage_unknown(&self) -> bool {
        self.attempts.last().is_some_and(|attempt| {
            attempt
                .usage_disposition()
                .is_unreported_after_interruption()
                && attempt.protocol_position != ProtocolPosition::NoResponse
        })
    }

    /// The call's usage, what a host meters it by: the sum its attempts'
    /// providers reported, counting the call in
    /// [`LlmUsage::unknown_calls`] when [`Self::usage_unknown`], so the
    /// counters are then a lower bound, never a total (ADR 0031). `Err`
    /// when the sum overflows.
    pub fn usage(&self) -> Result<LlmUsage, TokenUsageOverflow> {
        let unknown = self.usage_unknown().then(LlmUsage::unknown_call);
        self.attempts
            .iter()
            .filter_map(|attempt| attempt.usage.as_ref())
            .chain(unknown.as_ref())
            .try_fold(LlmUsage::default(), |total, usage| total.checked_add(usage))
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LlmResponse {
    /// Observed predicted continuation discarded by the protocol after its first cell.
    /// This note carries no model text and survives response normalization.
    /// Response derivation rebuilds it from the stream receipt; the protocol
    /// writes it to its extraction records.
    #[serde(skip)]
    pub discarded_after_cell: Option<DiscardedAfterCell>,
    pub parts: Vec<LlmOutputPart>,
    pub usage: LlmUsage,
    pub terminal_reason: LlmTerminalReason,
    /// Live terminal detail. Effect outcomes omit it from their journal form.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub terminal_diagnostic: Option<String>,
    pub provider_usage: Option<serde_json::Value>,
    pub request_body: Option<String>,
    pub http_summary: Option<String>,
    #[serde(default)]
    pub execution_evidence: Option<ExecutionEvidence>,
    /// Which of the caller's generation options the adapter put on this
    /// request's wire. `None` means the adapter does not report, which is
    /// distinct from a report that nothing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<GenerationReceipt>,
    /// Wire observations captured by the provider driver.
    ///
    /// `header:<lowercased-name>` and `body:<json-pointer>` keys are
    /// allowlisted by host-supplied endpoint configuration and are absent
    /// unless explicitly requested; lash holds no provider- or
    /// gateway-specific keys of its own (ADR 0038).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub response_metadata: std::collections::BTreeMap<String, serde_json::Value>,
    /// Whether the caller asked the provider to surface reasoning. Providers
    /// stamp this from the request's recorded `expose_thinking` default; reasoning parts
    /// stay in `parts` for multi-turn replay regardless, but the runtime only
    /// suppresses unstreamed reasoning blocks when a provider explicitly
    /// reports `Some(false)` — `None` (providers that do not stamp the flag,
    /// including third-party `Provider` impls) keeps the legacy publish
    /// behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_thinking: Option<bool>,
}

impl LlmResponse {
    /// Project the visible assistant prose from the response parts.
    ///
    /// The parts are the sole response-content authority. In particular, the
    /// projection applies provider phase visibility before concatenating text.
    pub fn full_text(&self) -> String {
        crate::turn_driver::visible_response_text_from_parts(&self.parts)
    }

    /// Stamp LLM Provider-owned replay state at the capture boundary without
    /// ever overwriting an existing, contradictory origin.
    pub fn stamp_replay_origin(
        &mut self,
        route: &ProviderRouteIdentity,
    ) -> Result<(), ProviderReplayOriginConflict> {
        if let Some(conflict) = self
            .parts
            .iter()
            .find_map(|part| part.replay_origin_conflict(route))
        {
            return Err(conflict);
        }
        for part in &mut self.parts {
            part.stamp_replay_origin(route)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LlmProfileSelection {
    pub model: &'static str,
    pub variant: Option<&'static str>,
}
