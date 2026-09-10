use super::*;

#[test]
fn session_usage_report_aggregates_sources_and_models() {
    let entries = vec![
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "gpt-5.4-mini".to_string(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            },
            usage_disposition: Default::default(),
        },
        TokenLedgerEntry {
            source: "observer".to_string(),
            model: "gpt-5.4-mini".to_string(),
            usage: TokenUsage {
                input_tokens: 7,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            usage_disposition: Default::default(),
        },
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "gpt-5.4".to_string(),
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 4,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            },
            usage_disposition: Default::default(),
        },
    ];

    let report = SessionUsageReport::from_entries(&entries);

    assert_eq!(report.entry_count, 3);
    assert!(!report.saturated);
    assert_eq!(report.usage.usage.input_tokens, 37);
    assert_eq!(report.usage.usage.output_tokens, 7);
    assert_eq!(report.usage.usage.cache_read_input_tokens, 8);
    assert_eq!(report.usage.usage.cache_write_input_tokens, 0);
    assert_eq!(report.usage.usage.reasoning_output_tokens, 3);
    assert_eq!(report.usage.total_tokens, 52);
    assert_eq!(report.by_source["turn"].usage.input_tokens, 30);
    assert_eq!(report.by_source["observer"].usage.output_tokens, 1);
    assert_eq!(report.by_model["gpt-5.4-mini"].usage.input_tokens, 17);
    assert_eq!(report.by_model["gpt-5.4"].usage.reasoning_output_tokens, 2);

    let delta = diff_token_ledger(
        &[TokenLedgerEntry {
            source: "turn".to_string(),
            model: "gpt-5.4-mini".to_string(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            },
            usage_disposition: Default::default(),
        }],
        &entries,
    )
    .expect("delta");
    assert_eq!(delta.len(), 2);
    assert_eq!(delta[0].source, "observer");
    assert_eq!(delta[1].model, "gpt-5.4");
}

#[test]
fn session_usage_report_saturates_accepted_rows_and_marks_the_projection() {
    let entries = vec![
        TokenLedgerEntry {
            source: "large".to_string(),
            model: "model-a".to_string(),
            usage: TokenUsage {
                input_tokens: i64::MAX,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
        },
        TokenLedgerEntry {
            source: "small".to_string(),
            model: "model-b".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
        },
    ];

    let report = SessionUsageReport::from_entries(&entries);

    assert!(report.saturated);
    assert_eq!(report.usage.usage.input_tokens, i64::MAX);
    assert_eq!(report.usage.total_tokens, i64::MAX);
}

#[test]
fn live_usage_merge_propagates_saturation_into_the_report() {
    let mut entries = vec![TokenLedgerEntry {
        source: "turn".to_string(),
        model: "model".to_string(),
        usage: TokenUsage {
            input_tokens: i64::MAX,
            ..TokenUsage::default()
        },
        usage_disposition: Default::default(),
    }];
    let saturated = merge_ledger_entry_saturating(
        &mut entries,
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "model".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
        },
    );

    let report = SessionUsageReport::from_entries_with_saturation(&entries, saturated);
    assert!(report.saturated);
    assert_eq!(report.by_source_model[0].usage.usage.input_tokens, i64::MAX);
}

fn usage(input_tokens: i64, output_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        output_tokens,
        ..TokenUsage::default()
    }
}

fn attempt(
    call_id: &str,
    attempt_ordinal: u32,
    generation_id: Option<&str>,
) -> crate::UnreportedLedgerAttempt {
    crate::UnreportedLedgerAttempt {
        call_id: call_id.to_string(),
        attempt_ordinal,
        generation_id: generation_id.map(str::to_string),
    }
}

#[test]
fn legacy_ledger_rows_decode_as_reported() {
    // Rows written before FIG-2765 carry no `usage_disposition`; they must
    // decode as provider-reported usage, and reported rows must serialize
    // byte-for-byte as before (the field is elided).
    let legacy = serde_json::json!({
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
    let entry: TokenLedgerEntry = serde_json::from_value(legacy.clone()).expect("legacy row");
    assert_eq!(entry.usage_disposition, LedgerUsageDisposition::Reported);
    assert_eq!(serde_json::to_value(&entry).expect("encode"), legacy);

    let hole = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "m".to_string(),
        usage: TokenUsage::default(),
        usage_disposition: LedgerUsageDisposition::unreported([
            attempt("call-9", 0, Some("gen-9")),
            attempt("call-7", 1, None),
        ]),
    };
    let encoded = serde_json::to_value(&hole).expect("encode hole");
    assert_eq!(
        encoded["usage_disposition"],
        serde_json::json!({
            "kind": "unreported",
            "attempts": [
                { "call_id": "call-7", "attempt_ordinal": 1 },
                { "call_id": "call-9", "attempt_ordinal": 0, "generation_id": "gen-9" }
            ]
        }),
        "holes serialize in canonical key order, absent generations elided"
    );
    let decoded_hole: TokenLedgerEntry = serde_json::from_value(encoded).expect("decode hole");
    assert_eq!(decoded_hole.usage_disposition, hole.usage_disposition);
    let correction = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "m".to_string(),
        usage: usage(120, 35),
        usage_disposition: LedgerUsageDisposition::Reconciled {
            call_id: "call-7".to_string(),
            attempt_ordinal: 1,
        },
    };
    let encoded = serde_json::to_value(&correction).expect("encode correction");
    assert_eq!(
        encoded["usage_disposition"],
        serde_json::json!({ "kind": "reconciled", "call_id": "call-7", "attempt_ordinal": 1 })
    );
    let decoded: TokenLedgerEntry = serde_json::from_value(encoded).expect("decode");
    assert_eq!(decoded.usage_disposition, correction.usage_disposition);
}

#[test]
fn usage_report_derives_outstanding_holes_from_unreported_and_reconciled_rows() {
    let entries = vec![
        TokenLedgerEntry::reported("turn", "m", usage(10, 2)),
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "m".to_string(),
            usage: TokenUsage::default(),
            usage_disposition: LedgerUsageDisposition::unreported([
                attempt("call-7", 1, Some("gen-7")),
                attempt("call-8", 0, None),
            ]),
        },
        TokenLedgerEntry {
            source: "turn".to_string(),
            model: "m".to_string(),
            usage: usage(120, 35),
            usage_disposition: LedgerUsageDisposition::Reconciled {
                call_id: "call-7".to_string(),
                attempt_ordinal: 1,
            },
        },
    ];
    let report = SessionUsageReport::from_entries(&entries);
    // Corrections sum into the totals; one of the two holes is filled.
    assert_eq!(report.usage.usage, usage(130, 37));
    assert_eq!(report.usage.total_tokens, 167);
    assert_eq!(report.usage.unreported_attempts, 1);
    assert_eq!(report.usage.reconciled_attempts, 1);
    assert_eq!(report.by_source["turn"].unreported_attempts, 1);
    assert_eq!(report.by_model["m"].reconciled_attempts, 1);
    assert_eq!(
        report.by_source_model.len(),
        3,
        "rows stay distinct per disposition"
    );
    let encoded = serde_json::to_value(&report.usage).expect("encode totals");
    assert_eq!(encoded["unreported_attempts"], 1);
    assert_eq!(encoded["reconciled_attempts"], 1);
    let reported_only = SessionUsageReport::from_entries(&entries[..1]);
    let encoded = serde_json::to_value(&reported_only.usage).expect("encode totals");
    assert!(
        encoded.get("unreported_attempts").is_none(),
        "zero counts are elided"
    );
    let decoded: UsageTotals = serde_json::from_value(encoded).expect("legacy totals decode");
    assert_eq!(decoded, reported_only.usage);
}

#[test]
fn ledger_merge_keeps_dispositions_apart_and_never_drops_a_hole() {
    let mut ledger = Vec::new();
    assert!(!merge_ledger_entry_saturating(
        &mut ledger,
        TokenLedgerEntry::reported("turn", "m", TokenUsage::default())
    ));
    assert!(ledger.is_empty(), "zero reported usage is still dropped");
    let hole = |call_id: &str| TokenLedgerEntry {
        source: "turn".to_string(),
        model: "m".to_string(),
        usage: TokenUsage::default(),
        usage_disposition: LedgerUsageDisposition::unreported([attempt(call_id, 0, None)]),
    };
    merge_ledger_entry_saturating(&mut ledger, hole("call-a"));
    merge_ledger_entry_saturating(&mut ledger, hole("call-b"));
    // Identity, not arithmetic: re-merging a hole already held is idempotent,
    // which is what lets a resident row and its durable twin both be folded.
    merge_ledger_entry_saturating(&mut ledger, hole("call-a"));
    merge_ledger_entry_saturating(
        &mut ledger,
        TokenLedgerEntry::reported("turn", "m", usage(5, 1)),
    );
    let correction = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "m".to_string(),
        usage: usage(7, 3),
        usage_disposition: LedgerUsageDisposition::Reconciled {
            call_id: "c".to_string(),
            attempt_ordinal: 1,
        },
    };
    merge_ledger_entry_saturating(&mut ledger, correction.clone());
    merge_ledger_entry_saturating(&mut ledger, correction);
    assert_eq!(
        ledger.len(),
        4,
        "holes fold, reported folds, corrections never merge"
    );
    assert_eq!(
        ledger[0].usage_disposition,
        LedgerUsageDisposition::unreported([
            attempt("call-a", 0, None),
            attempt("call-b", 0, None),
        ])
    );
    assert_eq!(ledger[1].usage, usage(5, 1));
    assert!(matches!(
        ledger[2].usage_disposition,
        LedgerUsageDisposition::Reconciled { .. }
    ));
    // Diffs fold every row of a key, so the correction counts as usage.
    let delta = diff_token_ledger(&[], &ledger).expect("diff");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].usage, usage(19, 7));
}

#[test]
fn pending_ledger_records_holes_and_corrections_separately_from_live_usage() {
    let ledger = Arc::new(Mutex::new(Vec::new()));
    session_manager::record_token_usage_shared(&ledger, "turn", "m", &usage(4, 4));
    session_manager::record_unreported_attempts_shared(&ledger, "turn", "m", &[]);
    assert_eq!(
        ledger.lock().expect("ledger").len(),
        1,
        "zero attempts write nothing"
    );
    session_manager::record_unreported_attempts_shared(
        &ledger,
        "turn",
        "m",
        &[attempt("call-1", 1, Some("gen-1"))],
    );
    session_manager::record_unreported_attempts_shared(
        &ledger,
        "turn",
        "m",
        &[attempt("call-2", 0, None)],
    );
    session_manager::record_token_usage_shared(&ledger, "turn", "m", &usage(1, 1));
    session_manager::record_reconciled_usage_shared(
        &ledger,
        "turn",
        "m",
        &usage(9, 9),
        "call-1",
        1,
    );
    let pending = ledger.lock().expect("ledger");
    assert_eq!(pending.len(), 3);
    assert_eq!(pending[0].entry.usage, usage(5, 5));
    assert_eq!(
        pending[0].entry.usage_disposition,
        LedgerUsageDisposition::Reported
    );
    assert_eq!(
        pending[1].entry.usage_disposition,
        LedgerUsageDisposition::unreported([
            attempt("call-1", 1, Some("gen-1")),
            attempt("call-2", 0, None),
        ])
    );
    assert_eq!(
        pending[2].entry.usage_disposition,
        LedgerUsageDisposition::Reconciled {
            call_id: "call-1".to_string(),
            attempt_ordinal: 1,
        }
    );
}
