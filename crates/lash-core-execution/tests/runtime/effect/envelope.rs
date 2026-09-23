mod settlement_order_journal_tests {
    use lash_core_execution::RuntimeEffectOutcome;

    /// A journaled tool-batch outcome is refused (FIG-3397): the batch command
    /// is gone, a batch is a durable effect group of tool-invocation children,
    /// and an entry recorded before that cutover must not decode into any
    /// current outcome.
    #[test]
    fn a_retired_tool_batch_outcome_does_not_decode() {
        let legacy = serde_json::json!({
            "type": "tool_batch",
            "launches": [],
            "settlement_order": [],
        });
        let error = serde_json::from_value::<RuntimeEffectOutcome>(legacy)
            .expect_err("a tool-batch outcome must not decode");
        assert!(
            error.to_string().contains("unknown variant `tool_batch`"),
            "the refusal must name the retired tag: {error}"
        );
    }

    /// FIG-2362: a journal entry written before the exec-code failure was typed
    /// journaled only the erased message string; it still decodes, under the
    /// honest `erased` reason.
    #[test]
    fn a_legacy_erased_exec_code_failure_still_decodes() {
        let legacy = serde_json::json!({
            "type": "exec_code",
            "result": { "Err": "code execution is not available in this session" },
        });
        let decoded = serde_json::from_value::<RuntimeEffectOutcome>(legacy)
            .expect("legacy erased exec-code failure decodes");
        let RuntimeEffectOutcome::ExecCode { result } = decoded else {
            panic!("decoded the wrong outcome kind");
        };
        let failure = result.expect_err("the journaled failure survives");
        assert_eq!(
            failure.reason,
            lash_core_execution::ExecCodeFailureReason::Erased
        );
        assert_eq!(
            failure.message,
            "code execution is not available in this session"
        );
    }
}
