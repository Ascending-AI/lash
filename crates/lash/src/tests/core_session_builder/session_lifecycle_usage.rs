use super::*;
use lash_sansio::SessionId;

fn turn_report(usage: TokenUsage, children_usage: Vec<TokenLedgerEntry>) -> TurnReport {
    use lash_core::{
        SessionPolicy, SessionSnapshot, facade_support::OutputState,
        facade_support::TurnExecutionMetrics, facade_support::TurnFinish,
        facade_support::TurnOutcome,
    };

    TurnReport {
        acceptance: None,
        cancel_input_outcome: Default::default(),
        state: SessionSnapshot {
            session_id: SessionId::from("s"),
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            ..lash_core::SessionSnapshot::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        },
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "ok".to_string(),
        }),
        assistant_output: AssistantOutput {
            safe_text: "ok".to_string(),
            raw_text: "ok".to_string(),
            state: OutputState::Usable,
        },
        usage,
        children_usage,
        llm_calls: Vec::new(),
        failure_evidence: Vec::new(),
        tool_calls: Vec::new(),
        omitted: None,
        execution: TurnExecutionMetrics::default(),
        errors: Vec::new(),
    }
}

#[test]
fn turn_result_total_usage_sums_parent_and_children() {
    let result = turn_report(
        TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 1,
        },
        vec![
            TokenLedgerEntry {
                source: "subagent".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 3,
                    reasoning_output_tokens: 0,
                },
                usage_disposition: Default::default(),
            },
            TokenLedgerEntry {
                source: "compaction".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 3,
                    reasoning_output_tokens: 0,
                },
                usage_disposition: Default::default(),
            },
        ],
    );

    let (total, saturated) = result.total_usage();
    assert!(!saturated);
    assert_eq!(total.input_tokens, 10 + 7 + 1);
    assert_eq!(total.output_tokens, 5 + 3);
    assert_eq!(total.cache_read_input_tokens, 2 + 4);
    assert_eq!(total.cache_write_input_tokens, 3 + 3 + 3);
    assert_eq!(total.reasoning_output_tokens, 1);
    // Parent's own usage is unchanged.
    assert_eq!(result.usage.input_tokens, 10);
}

#[test]
fn turn_result_total_usage_reports_saturation() {
    let result = turn_report(
        TokenUsage {
            input_tokens: i64::MAX,
            ..TokenUsage::default()
        },
        vec![TokenLedgerEntry {
            source: "subagent".to_string(),
            model: "m".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
        }],
    );

    let (total, saturated) = result.total_usage();
    assert!(saturated, "a clamped counter must surface the flag");
    assert_eq!(total.input_tokens, i64::MAX);
}
