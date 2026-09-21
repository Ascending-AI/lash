use super::*;

use crate::MessageRole;
use lash_sansio::core_support::ModelToolReturnCoreSupport;

fn attempt(ordinal: u32, input_tokens: i64) -> crate::AttemptRecord {
    attempt_with(
        ordinal,
        crate::AttemptOutcome::Completed,
        Some(input_tokens),
    )
}

fn attempt_with(
    ordinal: u32,
    outcome: crate::AttemptOutcome,
    input_tokens: Option<i64>,
) -> crate::AttemptRecord {
    crate::AttemptRecord {
        ordinal,
        started_at: 0,
        duration: std::time::Duration::ZERO,
        outcome,
        protocol_position: crate::ProtocolPosition::ResponseObserved,
        retry_budget_consumed: false,
        retry_decision: None,
        error: None,
        evidence: None,
        generation_disposition: None,
        usage: input_tokens.map(|input_tokens| crate::llm::types::LlmUsage {
            input_tokens,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        }),
        usage_disposition: crate::AttemptUsageDisposition::default(),
    }
}

fn call_record(id: &str, attempts: &[(u32, i64)]) -> crate::LlmCallRecord {
    crate::LlmCallRecord {
        call_id: LlmCallId(id.to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: attempts
            .iter()
            .map(|(ordinal, usage)| attempt(*ordinal, *usage))
            .collect(),
    }
}

fn call_record_of(id: &str, attempts: Vec<crate::AttemptRecord>) -> crate::LlmCallRecord {
    crate::LlmCallRecord {
        call_id: LlmCallId(id.to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts,
    }
}

fn spent(input: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        ..Default::default()
    }
}

fn message(content: &str) -> PluginMessage {
    PluginMessage {
        id: None,
        role: MessageRole::Assistant,
        content: content.to_string(),
        origin: None,
        parts: Vec::new(),
        attachments: Vec::new(),
    }
}

fn delta(attempt: u32, call_id: &str, provider_attempt: u32, usage: TokenUsage) -> ToolUsageDelta {
    ToolUsageDelta {
        attempt,
        llm_call_id: LlmCallId(call_id.to_string()),
        provider_attempt,
        usage,
    }
}

fn trigger() -> ToolTriggerEffectOutcome {
    ToolTriggerEffectOutcome {
        source_type: "timer".to_string(),
        source_key: "key".to_string(),
        occurrence_id: "occ".to_string(),
        payload: serde_json::json!({}),
        idempotency_key: "idem".to_string(),
        source: None,
        deliveries: Vec::new(),
    }
}

fn model_return() -> crate::ModelToolReturn {
    crate::ModelToolReturn::text("call".to_string(), "tool".to_string(), "ok")
}

fn settlement() -> ToolSettlement {
    ToolSettlement {
        version: TOOL_SETTLEMENT_VERSION,
        intent_outcomes: Vec::new(),
        possession: Vec::new(),
        triggers: Vec::new(),
        checkpoint_messages: Vec::new(),
        usage: Vec::new(),
        model_return: model_return(),
    }
}

/// Both durable carriers stamp this build's format version — a derived
/// `Default` (version `0`) would make every decode of an empty carrier refuse.
#[test]
fn version_stamps_are_stable() {
    assert_eq!(settlement().version, TOOL_SETTLEMENT_VERSION);
    assert_eq!(
        ToolAttemptCapture::default().version,
        TOOL_ATTEMPT_CAPTURE_VERSION
    );
    settlement()
        .validate()
        .expect("this build reads the settlement it writes");
    ToolAttemptCapture::default()
        .validate()
        .expect("this build reads the capture it writes");
}

/// The skip predicate the `ToolAttempt` outcome arm uses: any one fact makes
/// the capture worth journaling; none of them leaves the ungrouped outcome
/// corpus byte-identical.
#[test]
fn an_attempt_capture_is_empty_only_when_it_holds_no_fact_at_all() {
    assert!(ToolAttemptCapture::default().is_empty());
    assert!(
        !ToolAttemptCapture {
            messages: vec![message("committed")],
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !ToolAttemptCapture {
            usage: vec![delta(1, "call", 1, spent(3))],
            ..Default::default()
        }
        .is_empty(),
        "usage known at cancel is a fact and survives"
    );
}

/// A carrier this build cannot reconstruct is refused rather than served as a
/// prefix of what the child produced.
#[test]
fn a_carrier_from_another_format_version_is_refused() {
    let error = ToolSettlement {
        version: TOOL_SETTLEMENT_VERSION + 1,
        ..settlement()
    }
    .validate()
    .expect_err("a future settlement version is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolSettlementVersion
    );

    let error = ToolAttemptCapture {
        version: TOOL_ATTEMPT_CAPTURE_VERSION + 1,
        ..Default::default()
    }
    .validate()
    .expect_err("a future capture version is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolAttemptCaptureVersion
    );
}

/// Every field round-trips, and an unknown field is refused rather than
/// dropped: a carrier read as a subset of itself is the silent loss these
/// shapes exist to prevent.
#[test]
fn a_settlement_round_trips_and_refuses_an_unknown_field() {
    let settled = ToolSettlement {
        intent_outcomes: vec![crate::ToolIntentExecutionOutcome::Refused {
            identity: None,
            intent_index: 0,
            kind: crate::ToolIntentKind::StartProcess,
            refusal: crate::ToolIntentRefusalReason::MissingToolCallId,
        }],
        possession: vec![ProcessId::from("process:indexer")],
        triggers: vec![trigger()],
        checkpoint_messages: vec![message("committed")],
        usage: vec![delta(1, "call-7", 2, spent(11))],
        ..settlement()
    };
    let json = serde_json::to_string(&settled).expect("a settlement serializes");
    let decoded: ToolSettlement = serde_json::from_str(&json).expect("a settlement decodes");
    assert_eq!(decoded, settled);

    let mut value: serde_json::Value = serde_json::from_str(&json).expect("object");
    value["surprise"] = serde_json::json!(1);
    serde_json::from_value::<ToolSettlement>(value)
        .expect_err("an unknown field is refused, never dropped");
}

/// `model_return` has no serde default: a settlement without one means the
/// presentation boundary was never reached, which is a refusal the reader must
/// see rather than a hole to fill.
#[test]
fn a_settlement_without_a_model_return_refuses_to_decode() {
    let mut value = serde_json::to_value(settlement()).expect("encode");
    value.as_object_mut().unwrap().remove("model_return");
    serde_json::from_value::<ToolSettlement>(value)
        .expect_err("a settlement missing its presentation must not decode");
}

#[test]
fn an_attempt_capture_round_trips_and_refuses_an_unknown_field() {
    let capture = ToolAttemptCapture {
        version: TOOL_ATTEMPT_CAPTURE_VERSION,
        messages: vec![message("committed")],
        usage: vec![delta(2, "call-7", 1, spent(4))],
    };
    let json = serde_json::to_string(&capture).expect("a capture serializes");
    let decoded: ToolAttemptCapture = serde_json::from_str(&json).expect("a capture decodes");
    assert_eq!(decoded, capture);

    let mut value: serde_json::Value = serde_json::from_str(&json).expect("object");
    value["surprise"] = serde_json::json!(1);
    serde_json::from_value::<ToolAttemptCapture>(value)
        .expect_err("an unknown field is refused, never dropped");
}

/// §13: a spend is identified by its attempt and ADR 0032's `(LlmCallId,
/// provider-attempt ordinal)` pair so a re-attached fact can be recognised —
/// one fact per sealed provider attempt, so a billed failure and the retry
/// that replaced it each carry their own spend rather than a summed one.
#[test]
fn the_ledger_records_a_spend_with_its_full_identity() {
    let ledger = ToolUsageLedger::for_attempt(2);
    ledger.record(&call_record("call-a", &[(1, 3), (2, 5)]));
    assert_eq!(
        ledger.take(),
        vec![
            ToolUsageDelta {
                attempt: 2,
                llm_call_id: LlmCallId("call-a".to_string()),
                provider_attempt: 1,
                usage: spent(3),
            },
            ToolUsageDelta {
                attempt: 2,
                llm_call_id: LlmCallId("call-a".to_string()),
                provider_attempt: 2,
                usage: spent(5),
            },
        ]
    );
    assert!(
        ledger.take().is_empty(),
        "taking the ledger leaves it empty, so a second read cannot double-count"
    );
}

/// An aggregate ledger (`attempt` 0) takes journaled deltas back verbatim, so a
/// replayed attempt's capture restores into the child's settlement unchanged.
#[test]
fn the_aggregate_ledger_restores_journaled_deltas() {
    let ledger = ToolUsageLedger::new();
    ledger.extend(vec![
        delta(1, "call-a", 1, spent(3)),
        delta(2, "call-a", 2, spent(7)),
    ]);
    assert_eq!(
        ledger.take(),
        vec![
            delta(1, "call-a", 1, spent(3)),
            delta(2, "call-a", 2, spent(7))
        ]
    );
}

/// Unknown is a value and zero is a false fact (§13, ADR 0032). A provider
/// attempt reporting no usage contributes no row rather than a zero row.
#[test]
fn the_ledger_never_zero_fills() {
    let ledger = ToolUsageLedger::new();
    ledger.record(&call_record("call-a", &[(1, 0)]));
    assert!(
        ledger.take().is_empty(),
        "a call with no known usage must contribute no delta; only an explicit spend closes a hole"
    );
}

/// The ledger is shared, not copied: a clone handed to a nested future records
/// into the same accumulator the driver reads at the child's exit.
#[test]
fn a_cloned_ledger_records_into_the_same_accumulator() {
    let ledger = ToolUsageLedger::new();
    let nested = ledger.clone();
    nested.record(&call_record("call-a", &[(1, 2)]));
    assert_eq!(ledger.take().len(), 1);
}

/// §13's headline case: a provider attempt that billed and failed and the
/// retry that succeeded are two spends. The ledger keys a delta off the sealed
/// record's attempts — not the call's terminal outcome — so a billed failure
/// is kept next to, never instead of, the billed retry.
#[test]
fn a_billed_failed_attempt_and_its_successful_retry_are_two_facts() {
    let ledger = ToolUsageLedger::for_attempt(1);
    ledger.record(&call_record_of(
        "call-a",
        vec![
            attempt_with(1, crate::AttemptOutcome::Failed, Some(10)),
            attempt_with(2, crate::AttemptOutcome::Completed, Some(41)),
        ],
    ));
    assert_eq!(
        ledger.take(),
        vec![
            delta(1, "call-a", 1, spent(10)),
            delta(1, "call-a", 2, spent(41)),
        ]
    );
}

/// A call aborted after the provider billed it is still a spend: the sealed
/// record's attempt carries usage and the ledger keeps it — the capture
/// exists nowhere else once the error path returns.
#[test]
fn a_billed_aborted_attempt_is_a_fact() {
    let ledger = ToolUsageLedger::for_attempt(2);
    ledger.record(&call_record_of(
        "call-a",
        vec![attempt_with(1, crate::AttemptOutcome::Aborted, Some(9))],
    ));
    assert_eq!(ledger.take(), vec![delta(2, "call-a", 1, spent(9))]);
}

/// A failed attempt that never reached the provider reports `None` and
/// contributes no row: unbilled is a fact about billing, not a zero spend.
#[test]
fn an_unbilled_failed_attempt_records_nothing() {
    let ledger = ToolUsageLedger::new();
    ledger.record(&call_record_of(
        "call-a",
        vec![
            attempt_with(1, crate::AttemptOutcome::Failed, None),
            attempt_with(2, crate::AttemptOutcome::Completed, Some(3)),
        ],
    ));
    assert_eq!(
        ledger.take(),
        vec![delta(0, "call-a", 2, spent(3))],
        "only the billed attempt is a fact"
    );
}
