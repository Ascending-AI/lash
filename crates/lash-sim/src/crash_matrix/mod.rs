//! The crash matrix (ADR 0132 §14, design-opus §7.2): the production
//! durable runtime cut at every commit label.
//!
//! A cell is {case} × {commit label and occurrence} × {mode} × {seed}. A case
//! ([`Case`]) is one seam of the runtime: a turn, a tool round, a tool
//! round's store-local effects, a turn cancel, a code cell, a code cell
//! killed inside its body, a process with its waits and cascade, a
//! session close, a session's commands, a node's drain by release, a turn's prompt sections, a
//! session's compaction and a context-pressure frame. Each runs
//! as a [`deployment::Deployment`]: the production session and process
//! activations behind one dispatch on simulated nodes `a` and `b` over one
//! SQLite memory database, the host acting from outside through its own
//! producer store, all on one virtual clock.
//!
//! [`lash_durable_test::Matrix`] runs a case once to record the writes it
//! commits, then re-runs it cut at each one under each mode that means
//! something there, recovers on the other node, runs it to its end and
//! checks the [`invariants`] every cell keeps and the case's own laws. The
//! modes are the harness's faults: fail before the commit, the node killed
//! before it (crash) or after it (lost reply), the acknowledgement hidden or
//! delayed, a stale epoch and a zombie (commit, or hold the write, then
//! partition until the actors moved), and a lost wake.
//!
//! [`catalog_audit`] holds the matrix to the label catalog: every label in
//! [`lash_durable::CommitLabel::ALL`] is either committed by some case's
//! uncut run, and so cut by the matrix, or written down as one the runtime
//! does not emit, with the code that would.
//!
//! A seed varies how the run unfolds: which node starts and claims first,
//! and how long the slow tool takes. A seed reproduces one run of the same
//! runtime; nothing replays a recorded history.

pub mod cases;
pub mod catalog_audit;
pub mod cells;
pub mod compactions;
pub mod deployment;
pub mod effects;
pub mod engine;
pub mod findings;
pub mod invariants;
pub mod prompts;
pub mod services;
pub mod world;

use std::time::Duration;

use lash_durable::CommitLabel;
use lash_durable_test::{Fault, Matrix, MatrixReport, Write};

use deployment::{Deployment, Dialect, Workload};

/// The seam a case exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Case {
    /// A plain turn admitted, committed and released, and a node stopped
    /// cleanly after it.
    Turn,
    /// A turn's tool round: `Once` and `Repeatable` members, a retry and the
    /// presentation.
    Round,
    /// A turn's tool round whose members' store-local effects, a process
    /// start, commit with their outcomes.
    Effects,
    /// A turn cancelled by the host while its tool runs.
    Cancel,
    /// A turn's code cell: its snapshot, its `Once` operation and its
    /// resume from the snapshot.
    Cell,
    /// [`Self::Cell`] with the node killed inside the operation's body.
    CellKilled,
    /// A process's steps, its pinned key resolved by the host, its bounded
    /// wait on another process and its terminal's cascade.
    Process,
    /// A session closed by the host after its turn, over its `Until`
    /// process.
    Close,
    /// A session's host appends, applied by its actor as session commands:
    /// one appends, one settles `StaleBranch`.
    Command,
    /// A rolling deploy by release: the serving node drained while a
    /// process is parked, and its actors claimed by the next.
    Drain,
    /// A turn whose every model call composes plugin prompt sections: the
    /// call's prompt, its identity and the pending checkpoint decisions
    /// commit together at `model.start`, and a resend composes nothing.
    Prompt,
    /// A session's turn summarized by the host's compaction command: the
    /// summary call is admitted under `completion.start` with its exact
    /// body, and a resend sends that body.
    Compaction,
    /// A session whose first turn overflows: preparing its second turn
    /// summarizes the history and opens the recovery frame under
    /// `pressure.frame`, and the turn runs in it.
    Pressure,
}

impl Case {
    /// Every case, in registry order.
    pub const ALL: [Self; 13] = [
        Self::Turn,
        Self::Round,
        Self::Effects,
        Self::Cancel,
        Self::Cell,
        Self::CellKilled,
        Self::Process,
        Self::Close,
        Self::Command,
        Self::Drain,
        Self::Prompt,
        Self::Compaction,
        Self::Pressure,
    ];

    /// The case's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Round => "round",
            Self::Effects => "effects",
            Self::Cancel => "cancel",
            Self::Cell => "cell",
            Self::CellKilled => "cell_killed",
            Self::Process => "process",
            Self::Close => "close",
            Self::Command => "command",
            Self::Drain => "drain",
            Self::Prompt => "prompt",
            Self::Compaction => "compaction",
            Self::Pressure => "pressure",
        }
    }

    /// The case's work.
    #[must_use]
    pub fn workload(self) -> Box<dyn Workload> {
        self.workload_tagged("")
    }

    /// The case's work, its sessions named apart by `tag`, so several runs
    /// of it share one deployment.
    #[must_use]
    pub fn workload_tagged(self, tag: &str) -> Box<dyn Workload> {
        match self {
            Self::Turn => Box::new(cases::turn::TurnCase::tagged(tag)),
            Self::Round => Box::new(cases::round::RoundCase::tagged(tag)),
            Self::Effects => Box::new(cases::effects::EffectsCase::tagged(tag)),
            Self::Cancel => Box::new(cases::cancel::CancelCase::tagged(tag)),
            Self::Cell => Box::new(cases::cell::CellCase::tagged(false, tag)),
            Self::CellKilled => Box::new(cases::cell::CellCase::tagged(true, tag)),
            Self::Process => Box::<cases::process::ProcessCase>::default(),
            Self::Close => Box::new(cases::close::CloseCase::tagged(tag)),
            Self::Command => Box::new(cases::command::CommandCase::tagged(tag)),
            Self::Drain => Box::<cases::drain::DrainCase>::default(),
            Self::Prompt => Box::new(cases::prompt::PromptCase::tagged(tag)),
            Self::Compaction => Box::new(cases::compaction::CompactionCase::tagged(tag)),
            Self::Pressure => Box::new(cases::pressure::PressureCase::tagged(tag)),
        }
    }
}

/// The modes every cell is cut under (ADR 0132 §14): crash before the
/// commit, commit then crash, commit then partition (a stale epoch), a
/// zombie's held write, a hidden or delayed acknowledgement, a failure
/// before the commit and a lost wake. The harness offers each write only
/// the modes that mean something for its kind.
pub const MODES: [Fault; 8] = [
    Fault::FailBefore,
    Fault::Abort,
    Fault::CommitThenAbort,
    Fault::AckHidden,
    Fault::DelayedAck(Duration::from_secs(2)),
    Fault::StaleEpoch,
    Fault::Zombie,
    Fault::LostWake,
];

/// The virtual time a cell may take before the matrix calls it stalled.
const HORIZON: Duration = Duration::from_secs(600);

/// The seeds a sweep runs: `LASH_CRASH_MATRIX_SEEDS` of them (default 2),
/// from 0.
#[must_use]
pub fn seeds() -> Vec<u64> {
    // The optional double-profile proof can select one seed per action
    // without changing the ordinary sweep or its existing timeout.
    if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1")
        && let Some(seed) = std::env::var("LASH_CRASH_MATRIX_PROOF_SEED")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    {
        return vec![seed];
    }
    let count = std::env::var("LASH_CRASH_MATRIX_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse::<u64>().ok())
        .unwrap_or(2)
        .max(1);
    (0..count).collect()
}

/// One case's matrix at one seed.
pub struct CaseReport {
    pub case: Case,
    pub seed: u64,
    pub report: MatrixReport,
}

impl CaseReport {
    /// The distinct labels the uncut run committed.
    #[must_use]
    pub fn committed_labels(&self) -> Vec<CommitLabel> {
        committed_labels(&self.report.baseline)
    }
}

/// The distinct labels `writes` committed.
#[must_use]
pub fn committed_labels(writes: &[Write]) -> Vec<CommitLabel> {
    let mut labels: Vec<CommitLabel> = writes
        .iter()
        .filter(|write| write.committed())
        .map(|write| write.point.label)
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// Run `case`'s matrix at `seed` over `modes` on SQLite memory: the uncut
/// run, then every committed write cut under every mode that applies to it.
pub async fn run_case(case: Case, seed: u64, modes: &[Fault]) -> CaseReport {
    run_case_on(case, seed, modes, &Dialect::SqliteMemory).await
}

/// [`run_case`] over `dialect`.
pub async fn run_case_on(case: Case, seed: u64, modes: &[Fault], dialect: &Dialect) -> CaseReport {
    let parallelism = match std::env::var("LASH_MATRIX_THREADS") {
        Ok(value) => value
            .parse::<std::num::NonZeroUsize>()
            .unwrap_or_else(|error| {
                panic!("LASH_MATRIX_THREADS must be a positive integer: {error}")
            }),
        Err(std::env::VarError::NotPresent) => {
            std::thread::available_parallelism().unwrap_or(std::num::NonZeroUsize::MIN)
        }
        Err(error) => panic!("read LASH_MATRIX_THREADS: {error}"),
    };
    let matrix = Matrix::new()
        .parallelism(parallelism)
        .faults(modes)
        .horizon(HORIZON);
    let make = || Deployment::new(case, seed, dialect.clone());
    let report = if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
        matrix.run_lease_equivalence(make).await
    } else {
        matrix.run(make).await
    };
    CaseReport { case, seed, report }
}

/// Run `case`'s full matrix at every seed of [`seeds`] on SQLite memory,
/// panicking with every failed cell; answers the cells it ran per seed.
pub async fn assert_case(case: Case) -> Vec<(u64, usize)> {
    assert_case_on(case, &Dialect::SqliteMemory).await
}

/// [`assert_case`] over `dialect`.
pub async fn assert_case_on(case: Case, dialect: &Dialect) -> Vec<(u64, usize)> {
    let mut counts = Vec::new();
    for seed in seeds() {
        let run = run_case_on(case, seed, &MODES, dialect).await;
        let labels = run.report.labels();
        eprintln!(
            "crash matrix {} seed {seed}: {} cells over {} labels ({})",
            case.name(),
            run.report.cells.len(),
            labels.len(),
            labels
                .iter()
                .map(|label| label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        // A cell an open finding explains is reported, not failed; every
        // other cell must hold.
        let mut held = run.report.clone();
        held.cells
            .retain(|cell| match findings::explaining_cell(case, cell) {
                Some(finding) => {
                    eprintln!(
                        "crash matrix {} seed {seed}: {} {} shows open finding {} ({}): {}",
                        case.name(),
                        cell.point,
                        cell.fault,
                        finding.id,
                        finding.owner,
                        finding.summary
                    );
                    false
                }
                None => true,
            });
        if !held.failures().is_empty() {
            eprintln!(
                "crash matrix {} seed {seed} uncut: {}",
                case.name(),
                run.report
                    .baseline
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        held.assert_held();
        counts.push((seed, run.report.cells.len()));
    }
    counts
}
