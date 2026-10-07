//! Each global invariant catches a history broken on purpose.
//!
//! The base is a real history: the pending-tool scenario, whose tool defers, whose host resolves the completion and
//! whose turn commits the result. It keeps every invariant. Each test breaks
//! it the way its checker exists to catch and proves that checker fails it.

use super::*;

const SEED: u64 = 0x5eed_4086;

fn checker(invariant: &str) -> &'static dyn HistoryChecker {
    *CHECKERS
        .iter()
        .find(|checker| checker.invariant() == invariant)
        .unwrap_or_else(|| panic!("no checker `{invariant}`"))
}

/// `history` breaks `invariant` and only the checker for it says so.
fn assert_caught(history: &History, invariant: &str) -> Report {
    let report = check_with(history, &[checker(invariant)]);
    assert!(
        !report.passed(),
        "`{invariant}` passed a history broken for it: {}",
        report.summary()
    );
    assert!(
        report
            .violations
            .iter()
            .all(|violation| violation.invariant == invariant)
    );
    let failure = report.failure();
    assert!(failure.contains(&format!("{SEED:#018x}")), "{failure}");
    assert!(failure.contains(invariant), "{failure}");
    report
}

#[test]
fn soak_history_transcript_order_catches_reversed_markers() {
    let mut history = host_history();
    let report = check_with(&history, &[checker("transcript-order")]);
    assert!(report.passed(), "{}", report.failure());
    assert_eq!(report.observed, [("transcript-order", 1)]);
    history.stores[0].transcripts[0].messages.swap(0, 1);
    let report = assert_caught(&history, "transcript-order");
    assert!(report.failure().contains("host_op"));
    assert!(report.failure().contains("fault"));
    // Both inputs may be in one user message. Byte order remains observable.
    history.stores[0].transcripts[0].messages =
        vec![("user".to_owned(), "input:b;input:a;".to_owned())];
    assert_caught(&history, "transcript-order");
    history.stores[0].transcripts[0].messages[0].1 = "input:a;input:b;".to_owned();
    assert!(check_with(&history, &[checker("transcript-order")]).passed());
}

#[test]
fn a_quarantine_entry_covers_only_its_own_violation() {
    let violation = Violation::new("tool-call-identity", "id x named 2 logical calls");
    for entry in quarantine::QUARANTINE {
        assert!(!entry.name.is_empty() && !entry.reason.is_empty());
        assert!(
            CHECKERS
                .iter()
                .any(|checker| checker.invariant() == entry.invariant),
            "quarantine `{}` names no registered invariant",
            entry.name
        );
    }
    assert!(
        quarantine::covering("fixture/none", &violation).is_none_or(|entry| {
            entry.invariant == "tool-call-identity"
                && violation.detail.contains(entry.detail_contains)
        })
    );
}

#[test]
fn soak_history_empty_history_cannot_pass_vacuously() {
    let mut report = check(&History::new("chaos-soak", SEED));
    report.require_observed();
    assert!(!report.passed(), "{}", report.summary());
}

fn host_history() -> History {
    let mut history = History::new("chaos-soak", SEED);
    for run in ["a", "b"] {
        history.push(Fact::HostOp {
            op: HostOp::Send,
            session: "s".to_owned(),
            runs: vec![run.to_owned()],
            outcome: HostOutcome::Known,
        });
    }
    history.push(Fact::Fault {
        kind: FaultKind::Kill,
        detail: "fixture crash".to_owned(),
    });
    history.stores.push(StoreSnapshot {
        label: "engine".to_owned(),
        inputs: ["a", "b"]
            .into_iter()
            .map(|run| InputRow {
                table: "pending_turn_inputs".to_owned(),
                session: "s".to_owned(),
                id: lash_core::PendingTurnInputDraft::keyed_input_id(
                    &lash_core::SessionId::from("s"),
                    run,
                )
                .to_string(),
                state: Some("completed".to_owned()),
                admitted_run: Some(run.to_owned()),
                obligation_state: None,
            })
            .collect(),
        transcripts: vec![TranscriptSession {
            session: "s".to_owned(),
            messages: vec![
                ("user".to_owned(), "input:a;".to_owned()),
                ("user".to_owned(), "input:b;".to_owned()),
                ("assistant".to_owned(), "answer:a;answer:b;".to_owned()),
            ],
            ..Default::default()
        }],
        ..Default::default()
    });
    history
}
