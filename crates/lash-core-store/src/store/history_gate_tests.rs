//! Guard the runtime store against unbudgeted materializing history reads.

#[test]
fn history_reads_are_budgeted_or_predicates() {
    let operations = super::runtime_store_decorator::RUNTIME_STORE_OPERATIONS;
    let materialized = [
        "SessionWindowRead",
        "HistoryPage",
        "UsageLedgerPage",
        "FailureEvidencePage",
        "SessionNodeRecord",
        "SessionGraph",
    ];
    let mut checked = Vec::new();
    for operation in operations {
        if !materialized
            .iter()
            .any(|result| operation.returns.contains(result))
        {
            continue;
        }
        checked.push(operation.name);
        assert!(
            operation.name == "load_session_window"
                || operation.params.contains(&"HistoryBudget")
                || operation.params.contains(&"NonZeroU32"),
            "{} materializes history without a frame or explicit budget",
            operation.name,
        );
    }
    assert_eq!(
        checked,
        [
            "load_session_window",
            "load_ancestors",
            "load_usage_ledger_page",
            "load_failure_evidence_page",
        ],
        "the materializing history operation inventory changed"
    );

    // The first operation is an existence predicate. The second returns only
    // grouped counters and outstanding holes, with no historical payloads.
    let non_materializing = [
        ("contains_active_ancestor", "Result<bool, StoreError>"),
        (
            "load_usage_totals",
            "Result<crate::usage::SessionUsageTotals, StoreError>",
        ),
    ];
    for (name, answer) in non_materializing {
        let signature = operations
            .iter()
            .find(|operation| operation.name == name)
            .unwrap_or_else(|| panic!("missing non-materializing history operation {name}"));
        assert_eq!(
            signature.returns.replace(' ', ""),
            answer.replace(' ', ""),
            "{name} now returns content"
        );
    }
}
