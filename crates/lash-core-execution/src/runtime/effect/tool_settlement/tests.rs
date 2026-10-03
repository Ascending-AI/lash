use super::*;

use crate::MessageRole;
use lash_sansio::core_support::ModelToolReturnCoreSupport;

fn message(content: &str) -> PluginMessage {
    PluginMessage {
        id: None,
        role: MessageRole::Assistant,
        origin: None,
        parts: vec![crate::Part::text(String::new(), content.to_string(), None)],
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
    crate::ModelToolReturn::text("tool".to_string(), "ok")
}

fn settlement() -> ToolSettlement {
    ToolSettlement {
        version: TOOL_SETTLEMENT_VERSION,
        intent_outcomes: Vec::new(),
        possession: Vec::new(),
        triggers: Vec::new(),
        checkpoint_messages: Vec::new(),
        stream: crate::runtime::effect::RecordedChildStream::default(),
        model_return: model_return(),
    }
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
            refusal: crate::ToolIntentRefusalReason::IntentIndexOverflow,
        }],
        possession: vec![crate::process_id_for_test("process:indexer")],
        triggers: vec![trigger()],
        checkpoint_messages: vec![message("committed")],
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
    };
    let json = serde_json::to_string(&capture).expect("a capture serializes");
    let decoded: ToolAttemptCapture = serde_json::from_str(&json).expect("a capture decodes");
    assert_eq!(decoded, capture);

    let mut value: serde_json::Value = serde_json::from_str(&json).expect("object");
    value["surprise"] = serde_json::json!(1);
    serde_json::from_value::<ToolAttemptCapture>(value)
        .expect_err("an unknown field is refused, never dropped");
}
