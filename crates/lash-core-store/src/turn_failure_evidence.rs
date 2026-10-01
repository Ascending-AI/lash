//! Durable, non-transcript evidence from charge-safety-refused generations.

/// Maximum resident bytes for one failed generation's visible partial output.
///
/// The marker is included inside this budget. Billing and refusal fields are
/// typed separately and are never traded away to make room for output text.
pub(crate) const TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES: usize = 64 * 1024;

/// Marker appended when failed-generation output exceeds its residency bound.
pub(crate) const TURN_FAILURE_PARTIAL_OUTPUT_TRUNCATION_MARKER: &str = "\n[truncated]";

/// Byte-bounded visible output recovered from a failed provider generation.
///
/// This is settlement evidence, not a [`crate::Message`], graph node, or
/// prompt contribution. Hosts may inspect it and make their own continuation
/// decision; core never turns it into semantic context.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "residency", rename_all = "snake_case")]
pub enum TurnFailurePartialOutput {
    /// The complete visible partial output fit inside the residency bound.
    Complete { text: String },
    /// The visible partial output was clipped on a UTF-8 boundary and carries
    /// the explicit truncation marker.
    Truncated {
        text: String,
        original_byte_count: u64,
    },
}

impl TurnFailurePartialOutput {
    pub(crate) fn bounded(text: String) -> Self {
        if text.len() <= TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES {
            return Self::Complete { text };
        }

        let retained_budget = TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES
            .saturating_sub(TURN_FAILURE_PARTIAL_OUTPUT_TRUNCATION_MARKER.len());
        let mut boundary = retained_budget.min(text.len());
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let original_byte_count = u64::try_from(text.len()).unwrap_or(u64::MAX);
        let mut retained = text[..boundary].to_string();
        retained.push_str(TURN_FAILURE_PARTIAL_OUTPUT_TRUNCATION_MARKER);
        Self::Truncated {
            text: retained,
            original_byte_count,
        }
    }

    /// Visible partial output, including the marker when it was truncated.
    pub fn text(&self) -> &str {
        match self {
            Self::Complete { text } | Self::Truncated { text, .. } => text,
        }
    }

    /// Whether the resident output omits bytes from the failed generation.
    pub const fn is_truncated(&self) -> bool {
        matches!(self, Self::Truncated { .. })
    }
}

/// Typed refusal facts that made regeneration unsafe.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ChargeSafetyRefusalEvidence {
    /// FIG-2144's typed policy bound; no parallel refusal enum is introduced.
    pub denial_reason: crate::ChargeSafetyDenialReason,
    /// Furthest protocol position reached by the refused attempt.
    pub protocol_position: crate::ProtocolPosition,
    /// One-based unsafe retry number from FIG-2144's typed decision.
    pub attempt_number: u8,
    pub attempt_count: u32,
}

impl ChargeSafetyRefusalEvidence {
    /// The provider refusal code, derived from the retained denial facts.
    pub fn code(&self) -> lash_sansio::session_model::FailureCode {
        lash_sansio::session_model::FailureCode::lash(
            self.denial_reason.failure_code(self.protocol_position),
        )
    }
}

/// Durable component retained for one failed provider generation.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct TurnFailureEvidence {
    /// Visible partial output, absent when the adapter reported no partial
    /// response at all. Partial tool-call state is deliberately not executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_output: Option<TurnFailurePartialOutput>,
    /// Provider-reported usage billed for the abandoned generation.
    pub billed_usage: crate::llm::types::LlmUsage,
    /// Typed refusal and attempt facts.
    pub refusal: ChargeSafetyRefusalEvidence,
}

impl TurnFailureEvidence {
    pub fn from_llm_failure(
        error: &crate::sansio::LlmCallError,
        call_record: &crate::LlmCallRecord,
    ) -> Option<Self> {
        let attempt = call_record.attempts.iter().rev().find(|attempt| {
            matches!(
                attempt
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.charge_safety()),
                Some(crate::ChargeSafetyDecision::Denied { .. })
            )
        })?;
        let crate::ChargeSafetyDecision::Denied {
            attempt_number,
            reason,
            ..
        } = attempt
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety())?
        else {
            return None;
        };
        let partial_output = error
            .partial_response
            .as_deref()
            .map(|response| TurnFailurePartialOutput::bounded(response.full_text()));
        let billed_usage = error
            .partial_response
            .as_deref()
            .map(|response| response.usage.clone())
            .or_else(|| attempt.usage.clone())
            .unwrap_or_default();
        Some(Self {
            partial_output,
            billed_usage,
            refusal: ChargeSafetyRefusalEvidence {
                denial_reason: reason,
                protocol_position: attempt.protocol_position,
                attempt_number,
                attempt_count: u32::try_from(call_record.attempts.len()).unwrap_or(u32::MAX),
            },
        })
    }
}

/// Failure evidence attached to one durable turn-settlement receipt.
///
/// Session reads return settlements in ascending receipt commit-time order,
/// with `turn_id` as the deterministic tie-breaker on every store backend.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnFailureSettlement {
    /// Store operation key of the turn settlement that owns this component,
    /// not a turn identity: every backend populates it from
    /// [`crate::OperationId::storage_key`], which serializes the whole
    /// operation. Typing it as a turn identity would let it address the wrong
    /// thing at a typed turn seam, and the field name is durable, so it keeps
    /// both its name and its `String` type.
    pub turn_id: String,
    /// Failed generations settled by this turn, in protocol order. Runtime
    /// derivation admits at most one component per sealed provider attempt.
    pub evidence: Vec<TurnFailureEvidence>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_safety_evidence_stores_only_denial_facts() {
        let error = crate::sansio::LlmCallError {
            message: "failed generation".into(),
            retryable: false,
            kind: crate::llm::types::ProviderFailureKind::Stream,
            raw: None,
            code: None,
            terminal_reason: crate::llm::types::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        use crate::{ChargeSafetyDenialReason as Reason, ProtocolPosition as Position};
        let positions = [
            Position::NoResponse,
            Position::ResponseObserved,
            Position::OutputStarted,
            Position::TerminalObserved,
        ];
        for (reason, codes) in [
            (
                Reason::GuaranteeRequired,
                [
                    "unsafe_retry_without_transport_classification",
                    "unsafe_retry_after_response_observed",
                    "unsafe_retry_after_output_started",
                    "unsafe_retry_after_terminal_observed",
                ],
            ),
            (
                Reason::UnsafeRetryLimitExceeded,
                ["charge_safety_unsafe_retry_limit_exceeded"; 4],
            ),
            (
                Reason::DuplicateCostLimitExceeded,
                ["charge_safety_duplicate_cost_limit_exceeded"; 4],
            ),
        ] {
            for (position, code) in positions.into_iter().zip(codes) {
                let record: crate::LlmCallRecord = serde_json::from_value(serde_json::json!({
                    "call_id": "charge-safety-evidence-law",
                    "attempts": [{
                        "ordinal": 1,
                        "outcome": "failed",
                        "protocol_position": position,
                        "retry_budget_consumed": false,
                        "retry_decision": {
                            "outcome": "declined", "cause": "charge_safety",
                            "tokens_at_stake": 10, "attempt_number": 1,
                            "reason": reason,
                        }
                    }]
                }))
                .expect("typed denied call");
                let evidence = TurnFailureEvidence::from_llm_failure(&error, &record)
                    .expect("denial evidence");
                let mut wire = serde_json::to_value(&evidence).expect("encoded evidence");
                assert!(
                    wire["refusal"].get("code").is_none(),
                    "the code is derived, not stored: {wire}"
                );
                assert_eq!(evidence.refusal.code().to_string(), format!("lash:{code}"));
                assert_eq!(
                    serde_json::from_value::<TurnFailureEvidence>(wire.clone())
                        .expect("round trip"),
                    evidence
                );
                wire["refusal"]["code"] = serde_json::json!(format!("lash:{code}"));
                assert!(
                    serde_json::from_value::<TurnFailureEvidence>(wire).is_err(),
                    "the deleted redundant field is refused"
                );
            }
        }
    }

    #[test]
    fn partial_output_residency_is_byte_exact_at_and_over_the_bound() {
        let at_bound = "x".repeat(TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES);
        let complete = TurnFailurePartialOutput::bounded(at_bound.clone());
        assert_eq!(complete.text(), at_bound);
        assert!(!complete.is_truncated());

        let oversized = format!("{}é", "x".repeat(TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES));
        let truncated = TurnFailurePartialOutput::bounded(oversized.clone());
        assert!(truncated.is_truncated());
        assert!(
            truncated
                .text()
                .ends_with(TURN_FAILURE_PARTIAL_OUTPUT_TRUNCATION_MARKER)
        );
        assert!(truncated.text().len() <= TURN_FAILURE_PARTIAL_OUTPUT_MAX_BYTES);
        assert_eq!(
            truncated,
            TurnFailurePartialOutput::Truncated {
                text: truncated.text().to_string(),
                original_byte_count: oversized.len() as u64,
            }
        );
    }
}
