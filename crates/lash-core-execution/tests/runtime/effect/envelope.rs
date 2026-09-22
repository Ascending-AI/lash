mod settlement_order_journal_tests {
    use lash_core_execution::RuntimeEffectOutcome;

    /// A journal entry written before settlement order existed must be refused.
    ///
    /// This is the whole reason the field carries no serde default: an
    /// aggregate that rejects with its first *settled* rejection cannot tell a
    /// defaulted input order from a recorded one, so replaying an older entry
    /// as input order would silently reintroduce the bug the order fixes.
    #[test]
    fn a_tool_batch_outcome_without_settlement_order_fails_closed() {
        // The tag key is `type`, not `kind`: a payload keyed `kind` fails on the
        // *tag* and would pass this test while proving nothing about the field.
        let legacy = serde_json::json!({
            "type": "tool_batch",
            "launches": [],
            "triggers": [],
        });
        let decoded = serde_json::from_value::<RuntimeEffectOutcome>(legacy);
        let error = decoded.expect_err("an outcome without settlement order must not decode");
        assert!(
            error.to_string().contains("settlement_order"),
            "the refusal must name the missing field, not the tag: {error}"
        );
    }

    /// A current entry round-trips with its order intact.
    #[test]
    fn a_tool_batch_outcome_round_trips_its_settlement_order() {
        let outcome = RuntimeEffectOutcome::ToolBatch {
            launches: Vec::new(),
            triggers: Vec::new(),
            settlement_order: vec![2, 0, 1],
        };
        let encoded = serde_json::to_string(&outcome).expect("outcome encodes");
        let decoded =
            serde_json::from_str::<RuntimeEffectOutcome>(&encoded).expect("outcome decodes");
        let RuntimeEffectOutcome::ToolBatch {
            settlement_order, ..
        } = decoded
        else {
            panic!("decoded the wrong outcome kind");
        };
        assert_eq!(settlement_order, vec![2, 0, 1]);
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
