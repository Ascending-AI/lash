//! The crash matrix (ADR 0132 §14, design-opus §7.2): every case of
//! `lash_sim::crash_matrix` cut at every commit label of its uncut run,
//! under every mode, at every seed (`LASH_CRASH_MATRIX_SEEDS`, default 2),
//! on the production durable runtime over SQLite memory, and, selected by
//! the release gate, over a SQLite file and PostgreSQL. Each cell recovers
//! on the other node and keeps the invariants F1, F2, NR-1 to NR-4, the
//! fold, terminal-or-durable-wait and the deadline bound, and its case's
//! own laws. The catalog audit holds the cases to every label the runtime
//! emits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_durable::CommitLabel;
use lash_durable_test::{Fault, Matrix, MatrixReport, Scenario};
use lash_sim::crash_matrix::deployment::{Deployment, Dialect};
use lash_sim::crash_matrix::services::EXT_WRITE;
use lash_sim::crash_matrix::{Case, assert_case, assert_case_on, catalog_audit, run_case};

// Direct admission laws use the same optional equivalence proof as the catalog.
trait MatrixTestExt {
    async fn run_test<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport;
}

impl MatrixTestExt for Matrix {
    #[allow(
        clippy::disallowed_methods,
        reason = "the test host selects the optional lease proof from its action environment"
    )]
    async fn run_test<S: Scenario>(&self, make: impl Fn() -> S) -> MatrixReport {
        if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
            self.run_lease_equivalence(make).await
        } else {
            self.run(make).await
        }
    }
}

macro_rules! crash_matrix {
    ($($name:ident => $case:ident;)*) => {
        $(
            #[tokio::test]
            async fn $name() {
                let cells = assert_case(Case::$case).await;
                remaining_reproducer_cells(Case::$case).await;
                eprintln!("{}: cells per seed {cells:?}", Case::$case.name());
            }
        )*
    };
}

crash_matrix! {
    a_turn_cut_at_every_label_commits_once => Turn;
    a_tool_round_cut_at_every_label_runs_no_once_body_twice => Round;
    a_tool_s_process_start_cut_at_every_label_commits_only_with_its_outcome => Effects;
    a_turn_cancel_cut_at_every_label_ends_the_turn_once => Cancel;
    a_code_cell_cut_at_every_label_resumes_from_its_snapshot => Cell;
    a_code_cell_killed_in_its_body_cut_at_every_label_settles_interrupted => CellKilled;
    a_process_cut_at_every_label_resolves_waits_and_cascades_once => Process;
    a_session_close_cut_at_every_label_ends_at_its_tombstone => Close;
    a_session_command_cut_at_every_label_settles_once => Command;
    a_drained_node_cut_at_every_label_releases_its_actors_to_the_next_once => Drain;
    a_turn_s_prompt_sections_cut_at_every_label_commit_with_each_call_s_admission => Prompt;
    a_compaction_s_summary_cut_at_every_label_sends_its_admitted_body => Compaction;
    a_pressure_frame_cut_at_every_label_opens_once_and_its_turn_runs_in_it => Pressure;
}

/// A reduced catalog sweep can omit a hard-coded reproducer seed. Its
/// owning law then runs just that reproducer's cuts, preserving the cell
/// union without adding a full matrix for the omitted seed.
async fn remaining_reproducer_cells(case: Case) {
    let (required, faults, label) = match case {
        Case::Effects => (
            0..2,
            &[Fault::StaleEpoch, Fault::Zombie][..],
            CommitLabel::ROUND_OUTCOME,
        ),
        Case::Cell => (
            0..1,
            &[Fault::CommitThenAbort][..],
            CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT,
        ),
        _ => return,
    };
    let covered = lash_sim::crash_matrix::seeds();
    for seed in required.filter(|seed| !covered.contains(seed)) {
        let report = Matrix::new()
            .faults(faults)
            .labels(&[label])
            .run_test(|| Deployment::new(case, seed, Dialect::SqliteMemory))
            .await;
        assert!(
            !report.cells.is_empty(),
            "{case:?} seed {seed}: no {label} was cut"
        );
        report.assert_held();
    }
}

/// FIG-7348: the guest race helper cancels its losing direct sleep before
/// returning the winner. Recovery keeps that ModelRequested decision, the
/// winner and its turn alive; later cell closing cannot replace the origin.
#[tokio::test]
async fn a_tool_sleep_race_restores_its_winner_and_records_the_loser_cancelled() {
    let worlds = RefCell::new(Vec::new());
    let report = Matrix::new()
        .faults(&[Fault::CommitThenAbort])
        .labels(&[CommitLabel::CELL_INJECT])
        .run_test(|| {
            let deployment = Deployment::new(Case::Cell, 0, Dialect::SqliteMemory);
            worlds.borrow_mut().push(Arc::clone(deployment.world()));
            deployment
        })
        .await;
    assert!(!report.cells.is_empty(), "the loser's cancellation was cut");
    report.assert_held();
    let session = lash_sim::crash_matrix::services::session_id("cell-cell");
    let run = lash_sim::crash_matrix::cases::run_of(&session);
    // The uncut deployment comes first; each cut's deployment follows it.
    let mut decisions = BTreeMap::new();
    for cut in &report.cells {
        let (call, cancellation) = sleep_cancel_decision(&cut.writes);
        assert!(decisions.insert(call, cancellation).is_none());
    }
    let worlds = worlds.into_inner();
    for world in &worlds[1..] {
        let core = lash_sim::crash_matrix::cells::cell_core(world).expect("the cell's core");
        let durable = core
            .session(session.clone())
            .durable()
            .await
            .expect("the race's durable session");
        let calls = durable
            .read_turn_tool_calls(&run)
            .await
            .expect("the race's committed calls");
        let loser = calls
            .iter()
            .find(|call| call.tool == "typescript.sleep")
            .unwrap_or_else(|| panic!("the losing sleep is recorded: {calls:?}"));
        let lash::tools::ToolCallOutcome::Cancelled(cancelled) = &loser.output.outcome else {
            panic!("the race loser is cancelled: {loser:?}");
        };
        assert_eq!(
            cancelled.origin,
            Some(lash_core::CancelOrigin::ModelRequested)
        );
        if let Some(decision) = decisions.remove(&loser.call_id) {
            assert_eq!(
                *cancelled, decision,
                "recovery must retain the first decision"
            );
        }
        let transcript = durable.transcript().await.expect("the race's transcript");
        let cell = transcript
            .visible()
            .find_map(|entry| match &entry.item {
                lash::transcript::TranscriptItem::Cell(cell) => Some(cell),
                _ => None,
            })
            .expect("the race's committed cell");
        assert!(!cell.result.is_failed(), "{:?}", cell.result);
        assert_eq!(cell.prints.len(), 1, "{:?}", cell.prints);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                cell.prints[0].value.as_str().expect("console.log text")
            )
            .expect("the printed winner is JSON"),
            serde_json::json!({ "ok": true, "wrote": { "x": 7, "session": "cell-cell" } })
        );
    }
    assert!(
        decisions.is_empty(),
        "every cut's sleep decision was restored"
    );
}

fn sleep_cancel_decision(
    writes: &[lash_durable_test::Write],
) -> (lash_core::ToolCallId, lash_core::ToolCancellation) {
    use lash_core_execution::runtime::actor::round::graph::RoundFact;
    use lash_core_store::checkpoint_graph::GraphDecoder;
    use lash_core_store::tool_run::AttemptOutcome;
    use lash_durable::domain::RunFactKind as Kind;

    let mut decisions = Vec::new();
    for write in writes.iter().filter(|write| write.committed()) {
        let mut decoder = GraphDecoder::new(write.facts.as_ref(), write.facts.read_limits);
        for row in write
            .facts
            .rows
            .iter()
            .filter(|row| row.kind == Kind::XCancel)
        {
            let Ok(RoundFact::Cancel { cancellation, .. }) =
                RoundFact::load(&mut decoder, row.record)
            else {
                panic!("a cancellation row must hold its typed decision");
            };
            let Some(call) = row.call.clone() else {
                panic!("a cancellation decision must identify its call");
            };
            decisions.push((call, cancellation));
        }
    }
    let [(call, cancellation)] = decisions.as_slice() else {
        panic!("exactly one direct sleep cancellation decision: {decisions:?}");
    };
    assert_eq!(
        cancellation.origin,
        Some(lash_core::CancelOrigin::ModelRequested)
    );
    let mut starts = 0;
    let mut outcomes = 0;
    for write in writes.iter().filter(|write| write.committed()) {
        let mut decoder = GraphDecoder::new(write.facts.as_ref(), write.facts.read_limits);
        for row in write
            .facts
            .rows
            .iter()
            .filter(|row| row.call.as_ref() == Some(call))
        {
            match row.kind {
                Kind::XStart => starts += 1,
                Kind::XOutcome => {
                    let Ok(RoundFact::Outcome { output, .. }) =
                        RoundFact::load(&mut decoder, row.record)
                    else {
                        panic!("an outcome row must hold its typed outcome");
                    };
                    let AttemptOutcome::Cancelled {
                        cancellation: actual,
                        ..
                    } = output.outcome
                    else {
                        panic!("the losing sleep must settle cancelled");
                    };
                    assert_eq!(
                        &actual, cancellation,
                        "settlement must keep the first origin"
                    );
                    outcomes += 1;
                }
                Kind::Retry | Kind::XFinal | Kind::XFinalOutcome => {
                    panic!("the cancelled sleep cannot retry or commit a final: {row:?}");
                }
                _ => {}
            }
        }
    }
    assert_eq!((starts, outcomes), (1, 1), "one admission and one outcome");
    (call.clone(), cancellation.clone())
}

/// A stale-epoch cut at the cell's first save, which commits the model
/// response that asked for it with the call's admission
/// (`model.done+cell.snapshot+admit`, FIG-6000), runs no body on the old
/// owner: its admission commits, its node pauses past its lease, and the new owner
/// restores from the snapshot and settles the started `Once` `Interrupted`.
/// The old owner's acknowledged admission reaches it only past its
/// self-stop deadline, so the body never starts there, nor anywhere.
#[tokio::test]
async fn a_stale_epoch_cut_at_a_cell_admission_runs_no_body_on_the_old_owner() {
    // The full Cell matrix owns the focused cuts for its seeds.
    for seed in (0..8).filter(|seed| !lash_sim::crash_matrix::seeds().contains(seed)) {
        let worlds = RefCell::new(Vec::new());
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch])
            .labels(&[CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT])
            .run_test(|| {
                let deployment = Deployment::new(Case::Cell, seed, Dialect::SqliteMemory);
                worlds.borrow_mut().push(Arc::clone(deployment.world()));
                deployment
            })
            .await;
        assert!(
            !report.cells.is_empty(),
            "seed {seed}: no admission was cut"
        );
        // The first run is the uncut one; each cut's run follows in order.
        let worlds = worlds.into_inner();
        for (cell, world) in report.cells.iter().zip(&worlds[1..]) {
            let bodies = world.ledger().of_tool(EXT_WRITE);
            assert!(
                bodies.is_empty(),
                "seed {seed}: {} {}: {EXT_WRITE}'s body ran: {bodies:?}\n  {}",
                cell.point,
                cell.fault,
                cell.trace
            );
        }
        report.assert_held();
    }
}

/// A stale-epoch session command commits nothing (FIG-5230): the session
/// actor writes each command's head commit on its fenced transaction under
/// `session.command`, so a zombie owner whose write reaches the store only
/// after the session moved has it refused with `OwnershipLost`, a stale
/// owner commits nothing after its cut (F1), and the new owner applies each
/// command exactly once.
#[tokio::test]
async fn a_stale_epoch_session_command_commits_nothing() {
    // The full Command matrix owns the focused cuts for its seeds.
    for seed in (0..4).filter(|seed| !lash_sim::crash_matrix::seeds().contains(seed)) {
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch, Fault::Zombie])
            .labels(&[CommitLabel::SESSION_COMMAND])
            .run_test(|| Deployment::new(Case::Command, seed, Dialect::SqliteMemory))
            .await;
        assert!(
            report.cells.iter().any(|cell| cell.fault == Fault::Zombie),
            "seed {seed}: no session command was cut under a zombie owner"
        );
        report.assert_held();
    }
}

/// Every label the runtime emits is committed by some case's uncut run, so
/// the matrix cuts it: label coverage is 100%.
#[tokio::test]
async fn every_emitted_commit_label_is_cut_by_some_case() {
    let mut committed = BTreeMap::new();
    for case in Case::ALL {
        let run = run_case(case, 0, &[]).await;
        let labels = run.committed_labels();
        eprintln!(
            "{}: {}",
            case.name(),
            labels
                .iter()
                .map(|label| label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        committed.insert(case, labels);
    }
    let audit = catalog_audit::audit(&committed);
    assert!(
        audit.violations.is_empty(),
        "the catalog audit failed:\n{}",
        audit.violations.join("\n")
    );
    assert_eq!(
        audit.cut,
        audit.emitted,
        "label coverage is {:.1}%",
        audit.coverage_percent()
    );
}

/// Each case's matrix on PostgreSQL, one law per case: every cell opens an
/// isolated database, so one law over every case outlasts a test's bound.
/// The match keeps a law for every case.
macro_rules! crash_matrix_on_postgres {
    ($($name:ident => $case:ident;)*) => {
        $(
            #[tokio::test]
            #[ignore = "requires PostgreSQL; select inside a with-service.sh pg gate"]
            async fn $name() {
                let dialect =
                    Dialect::postgres_from_env().expect("LASH_POSTGRES_DATABASE_URL names a server");
                let cells = assert_case_on(Case::$case, &dialect).await;
                eprintln!("{} on PostgreSQL: cells per seed {cells:?}", Case::$case.name());
            }
        )*

        #[allow(dead_code)]
        fn every_case_has_a_postgres_law(case: Case) {
            match case {
                $(Case::$case)|* => {}
            }
        }
    };
}

crash_matrix_on_postgres! {
    the_crash_matrix_holds_on_postgres_for_a_turn => Turn;
    the_crash_matrix_holds_on_postgres_for_a_tool_round => Round;
    the_crash_matrix_holds_on_postgres_for_a_tool_s_effects => Effects;
    the_crash_matrix_holds_on_postgres_for_a_turn_cancel => Cancel;
    the_crash_matrix_holds_on_postgres_for_a_code_cell => Cell;
    the_crash_matrix_holds_on_postgres_for_a_killed_code_cell => CellKilled;
    the_crash_matrix_holds_on_postgres_for_a_process => Process;
    the_crash_matrix_holds_on_postgres_for_a_session_close => Close;
    the_crash_matrix_holds_on_postgres_for_a_session_command => Command;
    the_crash_matrix_holds_on_postgres_for_a_drained_node => Drain;
    the_crash_matrix_holds_on_postgres_for_a_prompt_composition => Prompt;
    the_crash_matrix_holds_on_postgres_for_a_compaction_s_summary => Compaction;
    the_crash_matrix_holds_on_postgres_for_a_pressure_frame => Pressure;
}

/// Each case's matrix over a SQLite database file, one law per case: the
/// tier a single-host deployment runs on, whose WAL and file locks SQLite
/// memory does not exercise. The release gate selects it.
macro_rules! crash_matrix_on_a_sqlite_file {
    ($($name:ident => $case:ident;)*) => {
        $(
            #[tokio::test]
            #[ignore = "the SQLite-file tier; the release gate selects it with --include-ignored"]
            async fn $name() {
                let cells = assert_case_on(Case::$case, &Dialect::SqliteFile).await;
                eprintln!("{} on a SQLite file: cells per seed {cells:?}", Case::$case.name());
            }
        )*

        #[allow(dead_code)]
        fn every_case_has_a_sqlite_file_law(case: Case) {
            match case {
                $(Case::$case)|* => {}
            }
        }
    };
}

crash_matrix_on_a_sqlite_file! {
    the_crash_matrix_holds_on_a_sqlite_file_for_a_turn => Turn;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_tool_round => Round;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_tool_s_effects => Effects;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_turn_cancel => Cancel;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_code_cell => Cell;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_killed_code_cell => CellKilled;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_process => Process;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_session_close => Close;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_session_command => Command;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_drained_node => Drain;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_prompt_composition => Prompt;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_compaction_s_summary => Compaction;
    the_crash_matrix_holds_on_a_sqlite_file_for_a_pressure_frame => Pressure;
}
