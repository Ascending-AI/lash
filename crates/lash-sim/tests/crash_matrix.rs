//! The crash matrix (ADR 0132 §14, design-opus §7.2): every case of
//! `lash_sim::crash_matrix` cut at every commit label of its uncut run,
//! under every mode, at every seed (`LASH_CRASH_MATRIX_SEEDS`, default 2),
//! on the production durable runtime over SQLite memory. Each cell recovers
//! on the other node and keeps the invariants F1, F2, NR-1 to NR-4, the
//! fold, terminal-or-durable-wait and the deadline bound, and its case's
//! own laws. The catalog audit holds the cases to every label the runtime
//! emits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_durable::CommitLabel;
use lash_durable_test::{Fault, Matrix};
use lash_sim::crash_matrix::deployment::{Deployment, Dialect};
use lash_sim::crash_matrix::services::EXT_WRITE;
use lash_sim::crash_matrix::{Case, assert_case, assert_case_on, catalog_audit, run_case};

macro_rules! crash_matrix {
    ($($name:ident => $case:ident;)*) => {
        $(
            #[tokio::test]
            async fn $name() {
                let cells = assert_case(Case::$case).await;
                eprintln!("{}: cells per seed {cells:?}", Case::$case.name());
            }
        )*
    };
}

crash_matrix! {
    a_turn_cut_at_every_label_commits_once => Turn;
    a_tool_round_cut_at_every_label_runs_no_once_body_twice => Round;
    a_turn_cancel_cut_at_every_label_ends_the_turn_once => Cancel;
    a_code_cell_cut_at_every_label_resumes_from_its_snapshot => Cell;
    a_code_cell_killed_in_its_body_cut_at_every_label_settles_interrupted => CellKilled;
    a_process_cut_at_every_label_resolves_waits_and_cascades_once => Process;
    a_process_signalled_and_cancelled_cut_at_every_label_ends_once => Signal;
    a_session_close_cut_at_every_label_ends_at_its_tombstone => Close;
    a_trigger_occurrence_cut_at_every_label_starts_each_delivery_once => Trigger;
    a_drained_node_cut_at_every_label_releases_its_actors_to_the_next_once => Drain;
}

/// A stale-epoch cut at `cell.snapshot+admit` runs no body on the old owner:
/// its admission commits, its node pauses past its lease, and the new owner
/// restores from the snapshot and settles the started `Once` `Interrupted`.
/// The old owner's acknowledged admission reaches it only past its
/// self-stop deadline, so the body never starts there, nor anywhere.
#[tokio::test]
async fn a_stale_epoch_cut_at_a_cell_admission_runs_no_body_on_the_old_owner() {
    for seed in 0..8 {
        let worlds = RefCell::new(Vec::new());
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch])
            .labels(&[CommitLabel::CELL_SNAPSHOT_ADMIT])
            .run(|| {
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

/// Every case's matrix on PostgreSQL.
#[tokio::test]
#[ignore = "requires PostgreSQL; select inside a with-service.sh pg gate"]
async fn the_crash_matrix_holds_on_postgres() {
    let dialect = Dialect::postgres_from_env().expect("LASH_POSTGRES_DATABASE_URL names a server");
    for case in Case::ALL {
        let cells = assert_case_on(case, &dialect).await;
        eprintln!("{} on PostgreSQL: cells per seed {cells:?}", case.name());
    }
}
