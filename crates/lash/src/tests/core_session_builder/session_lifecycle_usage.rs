#[test]
fn turn_result_total_usage_sums_parent_and_children() {
    use lash_core::{
        SessionPolicy, SessionSnapshot, facade_support::OutputState,
        facade_support::TurnExecutionMetrics, facade_support::TurnFinish,
        facade_support::TurnOutcome,
    };

    let result = TurnReport {
        acceptance: None,
        cancel_input_outcome: Default::default(),
        state: SessionSnapshot {
            session_id: "s".to_string(),
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
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        children_usage: vec![
            TokenLedgerEntry {
                source: "subagent".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
            },
            TokenLedgerEntry {
                source: "compaction".to_string(),
                model: "m".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
            },
        ],
        llm_calls: Vec::new(),
        failure_evidence: Vec::new(),
        tool_calls: Vec::new(),
        execution: TurnExecutionMetrics::default(),
        errors: Vec::new(),
    };

    let total = result.total_usage();
    assert_eq!(total.input_tokens, 10 + 7 + 1);
    assert_eq!(total.output_tokens, 5 + 3);
    assert_eq!(total.cache_read_input_tokens, 2 + 4);
    assert_eq!(total.reasoning_output_tokens, 1);
    // Parent's own usage is unchanged.
    assert_eq!(result.usage.input_tokens, 10);
}
