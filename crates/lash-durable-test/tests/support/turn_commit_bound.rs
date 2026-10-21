//! FIG-5954: the owner commits through its answer, with exact retained
//! boundaries: each model call's standalone receipt (FIG-6230) that neither
//! native admission (FIG-7083) nor the turn's commit (FIG-6955) carries, and
//! the named cell boundaries. Tracing adds no commit (FIG-6299).

use super::*;

/// Each of these boundaries precedes a consumer of recorded cell state. The
/// model response a cell enters from commits with the cell's first save
/// (FIG-6000).
const CELL_BOUNDARIES: [(CommitLabel, &str); 1] = [(
    CommitLabel::ROUND_OUTCOME,
    "the VM consumes only a recorded tool outcome",
)];
/// Code turns run checkpoint callbacks, so each provider send retains its
/// standalone receipt. Callback-free native receipts commit with admission
/// (FIG-7083) or the turn's commit (FIG-6955).
const RECEIPT: (CommitLabel, &str) = (
    CommitLabel::MODEL_ANSWER,
    "FIG-6230: a paid answer commits before its derivation, with native admission (FIG-7083), or with the turn's commit (FIG-6955)",
);

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
    assert_eq!(
        cell.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        if turn.code() {
            CELL_BOUNDARIES.map(|(label, _)| label).to_vec()
        } else {
            Vec::new()
        }
    );
    let (receipt, why) = RECEIPT;
    assert!(!why.is_empty());
    let receipts = owner.iter().filter(|found| **found == receipt).count();
    assert_eq!(
        receipts,
        if turn.code() { models } else { 0 },
        "one standalone receipt per provider send not carried by admission or turn commit: {why}"
    );
    for (label, why) in cell {
        assert!(!why.is_empty());
        assert_eq!(
            owner.iter().filter(|found| *found == label).count(),
            1,
            "a retained boundary occurs exactly once: {label}: {why}"
        );
    }
    eprintln!(
        "{turn:?} traced={traced}: commits={} outputs={outputs} K={} R={receipts} labels={owner:?}; later session maintenance={:?}",
        owner.len(),
        cell.len(),
        &all[answer + 1..]
    );
    assert!(
        owner.len() <= outputs + 1 + cell.len() + receipts,
        "{turn:?} traced={traced}: {} owner commits exceed {outputs} outputs + 1 + K={} + R={receipts}: {owner:?}",
        owner.len(),
        cell.len()
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
