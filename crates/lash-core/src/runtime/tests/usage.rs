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
        },
        TokenLedgerEntry {
            source: "small".to_string(),
            model: "model-b".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
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
        },
    );

    let report = SessionUsageReport::from_entries_with_saturation(&entries, saturated);
    assert!(report.saturated);
    assert_eq!(report.by_source_model[0].usage.usage.input_tokens, i64::MAX);
}
