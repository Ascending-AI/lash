use super::*;

use crate::{LlmCallId, MessageRole};

fn attempt(ordinal: u32) -> crate::AttemptRecord {
    crate::AttemptRecord {
        ordinal,
        started_at: 0,
        duration: std::time::Duration::ZERO,
        outcome: crate::AttemptOutcome::Completed,
        protocol_position: crate::ProtocolPosition::ResponseObserved,
        retry_budget_consumed: false,
        retry_decision: None,
        error: None,
        evidence: None,
        generation_disposition: None,
        usage: None,
        usage_disposition: crate::AttemptUsageDisposition::default(),
    }
}

fn call_record(id: &str, attempts: u32) -> crate::LlmCallRecord {
    crate::LlmCallRecord {
        call_id: LlmCallId(id.to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: (1..=attempts).map(attempt).collect(),
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

/// A default capture is readable by this build. It is what an outcome whose
/// capture was skipped on the wire decodes back into, so a derived `Default`
/// (version `0`) would make every empty child's outcome refuse at decode.
#[test]
fn a_default_capture_carries_this_build_s_version() {
    let capture = ToolChildCapture::default();
    assert_eq!(capture.version, TOOL_CHILD_CAPTURE_VERSION);
    assert!(capture.is_empty());
    capture
        .validate()
        .expect("a default capture is readable by the build that made it");
}

/// The skip predicate the outcome arm uses. Any one fact makes the capture
/// worth writing; none of them makes it free.
#[test]
fn a_capture_is_empty_only_when_it_holds_no_fact_at_all() {
    assert!(ToolChildCapture::default().is_empty());
    assert!(
        !ToolChildCapture {
            possession: vec![ProcessId::from("process:indexer")],
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !ToolChildCapture {
            checkpoint_messages: vec![message("committed")],
            ..Default::default()
        }
        .is_empty()
    );
    assert!(
        !ToolChildCapture {
            usage: vec![ToolChildUsageFact {
                llm_call_id: LlmCallId("call".to_string()),
                provider_attempts: 1,
                usage: spent(3),
            }],
            ..Default::default()
        }
        .is_empty()
    );
}

/// A capture this build cannot reconstruct is refused rather than served as a
/// prefix of what the child produced.
#[test]
fn a_capture_from_another_format_version_is_refused() {
    let error = ToolChildCapture {
        version: TOOL_CHILD_CAPTURE_VERSION + 1,
        ..Default::default()
    }
    .validate()
    .expect_err("a future capture version is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCaptureVersion
    );
}

/// Every field round-trips, and an unknown field is refused rather than
/// dropped: a capture read as a subset of itself is the silent loss this type
/// exists to prevent.
#[test]
fn a_capture_round_trips_and_refuses_an_unknown_field() {
    let capture = ToolChildCapture {
        version: TOOL_CHILD_CAPTURE_VERSION,
        possession: vec![ProcessId::from("process:indexer"), ProcessId::from("p2")],
        checkpoint_messages: vec![message("committed")],
        usage: vec![ToolChildUsageFact {
            llm_call_id: LlmCallId("call-7".to_string()),
            provider_attempts: 2,
            usage: spent(11),
        }],
    };
    let json = serde_json::to_string(&capture).expect("a capture serializes");
    let decoded: ToolChildCapture = serde_json::from_str(&json).expect("a capture decodes");
    assert_eq!(decoded, capture);

    let mut value: serde_json::Value = serde_json::from_str(&json).expect("object");
    value["surprise"] = serde_json::json!(1);
    serde_json::from_value::<ToolChildCapture>(value)
        .expect_err("an unknown field is refused, never dropped");
}

/// §13: a spend the child made is recorded whether or not the child went on to
/// produce a value, and it is identified by ADR 0032's `(LlmCallId,
/// provider-attempt ordinal)` pair so a re-attached fact can be recognised.
#[test]
fn the_ledger_records_a_spend_with_its_provider_attempt_identity() {
    let ledger = ToolChildUsageLedger::new();
    ledger.record(&call_record("call-a", 2), &spent(5));
    ledger.record(&call_record("call-b", 1), &spent(7));
    let facts = ledger.take();
    assert_eq!(
        facts,
        vec![
            ToolChildUsageFact {
                llm_call_id: LlmCallId("call-a".to_string()),
                provider_attempts: 2,
                usage: spent(5),
            },
            ToolChildUsageFact {
                llm_call_id: LlmCallId("call-b".to_string()),
                provider_attempts: 1,
                usage: spent(7),
            },
        ]
    );
    assert!(
        ledger.take().is_empty(),
        "taking the ledger leaves it empty, so a second read cannot double-count"
    );
}

/// Unknown is a value and zero is a false fact (§13, ADR 0032). A call with no
/// known usage contributes no row rather than a zero row.
#[test]
fn the_ledger_never_zero_fills() {
    let ledger = ToolChildUsageLedger::new();
    ledger.record(&call_record("call-a", 1), &TokenUsage::default());
    assert!(
        ledger.take().is_empty(),
        "a call with no known usage must contribute no fact; only an explicit spend closes a hole"
    );
}

/// The ledger is shared, not copied: a clone handed to a nested future records
/// into the same accumulator the driver reads at the child's exit.
#[test]
fn a_cloned_ledger_records_into_the_same_accumulator() {
    let ledger = ToolChildUsageLedger::new();
    let nested = ledger.clone();
    nested.record(&call_record("call-a", 1), &spent(2));
    assert_eq!(ledger.take().len(), 1);
}
