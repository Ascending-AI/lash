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
    a_tool_s_process_start_and_signal_cut_at_every_label_commit_only_with_its_outcome => Effects;
    a_turn_cancel_cut_at_every_label_ends_the_turn_once => Cancel;
    a_code_cell_cut_at_every_label_resumes_from_its_snapshot => Cell;
    a_code_cell_killed_in_its_body_cut_at_every_label_settles_interrupted => CellKilled;
    a_process_cut_at_every_label_resolves_waits_and_cascades_once => Process;
    a_process_signalled_and_cancelled_cut_at_every_label_ends_once => Signal;
    a_session_close_cut_at_every_label_ends_at_its_tombstone => Close;
    a_session_command_cut_at_every_label_settles_once => Command;
    a_trigger_occurrence_cut_at_every_label_starts_each_delivery_once => Trigger;
    a_drained_node_cut_at_every_label_releases_its_actors_to_the_next_once => Drain;
    a_turn_s_prompt_sections_cut_at_every_label_commit_with_each_call_s_admission => Prompt;
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

/// A stale-epoch cut at `label` under `case`, at eight seeds over SQLite
/// memory, holds every invariant, the lease law among them. The paused node
/// resumes as soon as its actors moved, and the activations its held reply
/// wakes run before its runner's next tick: the resumed node carries on
/// past its self-stop deadline while the new owner already runs its actors.
async fn assert_stale_epoch_holds(case: Case, label: CommitLabel) {
    for seed in 0..8 {
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch])
            .labels(&[label])
            .activations_resume_first()
            .run(|| Deployment::new(case, seed, Dialect::SqliteMemory))
            .await;
        assert!(!report.cells.is_empty(), "seed {seed}: no {label} was cut");
        report.assert_held();
    }
}

/// A stale-epoch cut at `model.done` runs no round member's body on the old
/// owner: the round's admission commits, its node pauses past its lease,
/// and the new owner settles each started `Once` `Interrupted`. The old
/// owner's acknowledged admission reaches its round runner only past its
/// self-stop deadline, so no member's body starts there.
#[tokio::test]
async fn a_stale_epoch_cut_at_a_round_admission_runs_no_member_body_on_the_old_owner() {
    assert_stale_epoch_holds(Case::Round, CommitLabel::MODEL_DONE).await;
}

/// A stale-epoch cut at `process.advance` runs no process step's body on
/// the old owner: the transition that admits the steps commits, its node
/// pauses past its lease, and the new owner settles the started `Once`
/// step `Interrupted`; no step's body starts on the old owner.
#[tokio::test]
async fn a_stale_epoch_cut_at_a_step_admission_runs_no_step_body_on_the_old_owner() {
    assert_stale_epoch_holds(Case::Process, CommitLabel::PROCESS_ADVANCE).await;
}

/// A stale-epoch session command commits nothing (FIG-5230): the session
/// actor writes each command's head commit on its fenced transaction under
/// `session.command`, so a zombie owner whose write reaches the store only
/// after the session moved has it refused with `OwnershipLost`, a stale
/// owner commits nothing after its cut (F1), and the new owner applies each
/// command exactly once.
#[tokio::test]
async fn a_stale_epoch_session_command_commits_nothing() {
    for seed in 0..4 {
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch, Fault::Zombie])
            .labels(&[CommitLabel::SESSION_COMMAND])
            .run(|| Deployment::new(Case::Command, seed, Dialect::SqliteMemory))
            .await;
        assert!(
            report.cells.iter().any(|cell| cell.fault == Fault::Zombie),
            "seed {seed}: no session command was cut under a zombie owner"
        );
        report.assert_held();
    }
}

/// A stale owner's or a zombie's `round.outcome` commit, carrying a tool's
/// process start and signal, commits nothing: the new owner settles both
/// `Once` calls `Interrupted`, and neither the process nor the signal
/// exists without its outcome (the case's laws).
#[tokio::test]
async fn a_stale_owner_s_tool_outcome_with_a_process_start_commits_nothing() {
    for seed in 0..2 {
        let report = Matrix::new()
            .faults(&[Fault::StaleEpoch, Fault::Zombie])
            .labels(&[CommitLabel::ROUND_OUTCOME])
            .run(|| Deployment::new(Case::Effects, seed, Dialect::SqliteMemory))
            .await;
        assert!(
            !report.cells.is_empty(),
            "seed {seed}: no round.outcome was cut"
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
    the_crash_matrix_holds_on_postgres_for_a_signalled_process => Signal;
    the_crash_matrix_holds_on_postgres_for_a_session_close => Close;
    the_crash_matrix_holds_on_postgres_for_a_session_command => Command;
    the_crash_matrix_holds_on_postgres_for_a_trigger_occurrence => Trigger;
    the_crash_matrix_holds_on_postgres_for_a_drained_node => Drain;
    the_crash_matrix_holds_on_postgres_for_a_prompt_composition => Prompt;
}
