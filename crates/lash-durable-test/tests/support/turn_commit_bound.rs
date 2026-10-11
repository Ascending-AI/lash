//! FIG-5954: the owner commits through its answer, with exact retained boundaries.

use super::*;

/// Each of these boundaries precedes a consumer of recorded cell state.
const CELL_BOUNDARIES: [(CommitLabel, &str); 3] = [
    (
        CommitLabel::MODEL_DONE,
        "the VM enters from the recorded model response",
    ),
    (
        CommitLabel::ROUND_OUTCOME,
        "the VM consumes only a recorded tool outcome",
    ),
    (
        CommitLabel::CELL_SNAPSHOT,
        "the turn consumes only a recorded cell end",
    ),
];
const TRACE_BOUNDARIES: [(CommitLabel, &str); 2] = [
    (
        CommitLabel::TURN_TRACED,
        "FIG-5457: acknowledge the exported turn admission across owner replacement",
    ),
    (
        CommitLabel::ROUND_TRACED,
        "FIG-5452: acknowledge exported tool admissions across owner replacement",
    ),
];

async fn counted(turn: Turn, traced: bool, cell: &[(CommitLabel, &str)]) {
    let fixtures = Mutex::new(Vec::new());
    let report = Matrix::new()
        .faults(&[])
        .run_test(|| {
            let mut crash = Crash::new(turn, Dialect::SqliteMemory, None);
            crash.trace_adapter = traced;
            fixtures
                .lock_recover()
                .push((Arc::clone(&crash.world), Arc::clone(&crash.scripts)));
            crash
        })
        .await;
    report.assert_held();
    let all: Vec<_> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    let answer = all
        .iter()
        .position(|label| *label == CommitLabel::TURN_COMMIT)
        .expect("the turn committed its host answer");
    let owner = &all[..=answer];
    let fixtures = fixtures.lock_recover();
    let models = fixtures[0].1.requests(INPUT).len();
    let bodies = fixtures[0]
        .0
        .entries()
        .values()
        .map(Vec::len)
        .sum::<usize>();
    let outputs = models + bodies + 1; // actual provider sends, body entries, host answer
    assert_eq!(models, if turn == Turn::Plain { 1 } else { 2 });
    assert_eq!(bodies, usize::from(turn != Turn::Plain));
    let trace = if traced {
        &TRACE_BOUNDARIES[..if turn == Turn::Plain { 1 } else { 2 }]
    } else {
        &[]
    };
    assert_eq!(
        cell.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        if turn.code() {
            CELL_BOUNDARIES.map(|(label, _)| label).to_vec()
        } else {
            Vec::new()
        }
    );
    assert_eq!(
        owner
            .iter()
            .filter(|label| TRACE_BOUNDARIES.iter().any(|(trace, _)| trace == *label))
            .copied()
            .collect::<Vec<_>>(),
        trace.iter().map(|(label, _)| *label).collect::<Vec<_>>()
    );
    for (label, why) in cell.iter().chain(trace) {
        assert!(!why.is_empty());
        assert_eq!(
            owner.iter().filter(|found| *found == label).count(),
            1,
            "a retained boundary occurs exactly once: {label}: {why}"
        );
    }
    eprintln!(
        "{turn:?} traced={traced}: commits={} outputs={outputs} K={} T={} labels={owner:?}; later session maintenance={:?}",
        owner.len(),
        cell.len(),
        trace.len(),
        &all[answer + 1..]
    );
    assert!(
        owner.len() <= outputs + 1 + cell.len() + trace.len(),
        "{turn:?} traced={traced}: {} owner commits exceed {outputs} outputs + 1 + K={} + T={}: {owner:?}",
        owner.len(),
        cell.len(),
        trace.len()
    );
}

#[tokio::test]
async fn a_no_tool_turn_commits_at_most_once_per_output_plus_one() {
    for traced in [false, true] {
        counted(Turn::Plain, traced, &[]).await;
    }
}

#[tokio::test]
async fn a_one_tool_turn_commits_at_most_once_per_output_plus_one() {
    for traced in [false, true] {
        counted(Turn::Trace, traced, &[]).await;
    }
}

#[tokio::test]
async fn a_one_park_cell_turn_keeps_only_its_named_recorded_boundaries() {
    for traced in [false, true] {
        counted(Turn::CellTrace, traced, &CELL_BOUNDARIES).await;
    }
}
