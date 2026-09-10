//! FIG-2765: window-57 usage dispositions stay additive over window-55 rows.

use super::*;

#[test]
fn window_57_usage_dispositions_are_additive_over_window_55_rows() {
    let legacy_row = serde_json::json!({
        "source": "turn",
        "model": "m",
        "usage": {
            "input_tokens": 3,
            "output_tokens": 4,
            "cache_read_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "reasoning_output_tokens": 0
        }
    });
    let row: RemoteTokenLedgerEntry =
        serde_json::from_value(legacy_row.clone()).expect("window-55 ledger row");
    assert_eq!(
        row.usage_disposition,
        RemoteLedgerUsageDisposition::Reported
    );
    assert_eq!(serde_json::to_value(&row).expect("encode"), legacy_row);

    let hole = RemoteTokenLedgerEntry {
        usage_disposition: RemoteLedgerUsageDisposition::Unreported {
            attempts: vec![
                RemoteUnreportedLedgerAttempt {
                    call_id: "call-1".to_string(),
                    attempt_ordinal: 0,
                    generation_id: Some("gen-1".to_string()),
                },
                RemoteUnreportedLedgerAttempt {
                    call_id: "call-2".to_string(),
                    attempt_ordinal: 3,
                    generation_id: None,
                },
            ],
        },
        ..row.clone()
    };
    assert_eq!(
        serde_json::to_value(&hole).expect("encode hole")["usage_disposition"],
        serde_json::json!({
            "kind": "unreported",
            "attempts": [
                { "call_id": "call-1", "attempt_ordinal": 0, "generation_id": "gen-1" },
                { "call_id": "call-2", "attempt_ordinal": 3 }
            ]
        })
    );
    let decoded_hole: RemoteTokenLedgerEntry =
        serde_json::from_value(serde_json::to_value(&hole).expect("encode hole"))
            .expect("decode hole");
    assert_eq!(decoded_hole, hole);
    // The core round trip keeps every descriptor field, absent generation ids
    // included: the wire mirror is what a remote host reconciles from.
    let core: lash_core::LedgerUsageDisposition = hole.usage_disposition.clone().into();
    assert_eq!(
        RemoteLedgerUsageDisposition::from(core),
        hole.usage_disposition
    );
    let correction = RemoteTokenLedgerEntry {
        usage_disposition: RemoteLedgerUsageDisposition::Reconciled {
            call_id: "call-1".to_string(),
            attempt_ordinal: 2,
        },
        ..row
    };
    let encoded = serde_json::to_value(&correction).expect("encode correction");
    assert_eq!(
        encoded["usage_disposition"],
        serde_json::json!({ "kind": "reconciled", "call_id": "call-1", "attempt_ordinal": 2 })
    );
    let decoded: RemoteTokenLedgerEntry = serde_json::from_value(encoded).expect("decode");
    assert_eq!(decoded, correction);

    let legacy_attempt = serde_json::json!({
        "ordinal": 1,
        "started_at_ms": 0,
        "duration_ms": 0,
        "outcome": "aborted",
        "protocol_position": "output_started",
        "retry_budget_consumed": true
    });
    let attempt: RemoteAttemptRecord =
        serde_json::from_value(legacy_attempt.clone()).expect("window-55 attempt");
    assert_eq!(
        attempt.usage_disposition,
        RemoteAttemptUsageDisposition::Reported
    );
    assert_eq!(
        serde_json::to_value(&attempt).expect("encode"),
        legacy_attempt
    );
    let aborted = RemoteAttemptRecord {
        usage_disposition: RemoteAttemptUsageDisposition::UnreportedAfterAbort,
        ..attempt
    };
    assert_eq!(
        serde_json::to_value(&aborted).expect("encode")["usage_disposition"],
        "unreported_after_abort"
    );
}
