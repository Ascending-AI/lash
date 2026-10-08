//! The label catalog audit: the matrix covers every commit label the
//! runtime emits.
//!
//! [`COVERAGE`] names, for every label of [`CommitLabel::ALL`], either the
//! cases whose uncut run commits it (the matrix cuts every committed write
//! of an uncut run, so those cases cut it under every mode that applies), or
//! why the runtime emits no commit under it and which code would. [`audit`]
//! reads the uncut runs and refuses:
//!
//! - a catalog label with no row, or a row for a label the catalog lacks;
//! - a label a row names cases for that one of those cases did not commit;
//! - a label a row calls unemitted that some case committed, since that
//!   label is emitted and must be covered.
//!
//! Coverage is then every emitted label: 100% of the labels the runtime
//! commits under are cut.

use std::collections::BTreeMap;

use lash_durable::CommitLabel;

use super::Case;

/// How the matrix covers one label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// These cases' uncut runs commit it, so their matrices cut it.
    Cases(&'static [Case]),
    /// No production code commits under it: the reason, naming who would.
    Unemitted(&'static str),
}

/// Every catalog label and how the matrix covers it.
pub const COVERAGE: [(CommitLabel, Coverage); 46] = [
    (CommitLabel::CLAIM, Coverage::Cases(&[Case::Turn])),
    (CommitLabel::HEARTBEAT, Coverage::Cases(&[Case::Turn])),
    (CommitLabel::REAP, Coverage::Cases(&[Case::CellKilled])),
    (CommitLabel::NODE_REGISTER, Coverage::Cases(&[Case::Turn])),
    (CommitLabel::NODE_RELEASE, Coverage::Cases(&[Case::Turn])),
    (CommitLabel::NODE_DRAIN, Coverage::Cases(&[Case::Drain])),
    (
        CommitLabel::TURN_ACCEPT,
        Coverage::Unemitted(
            "session work arrives as mail (mail.session, FIG-5196) and the session's drain admits it with its turn under turn.admit; no production code commits under turn.accept",
        ),
    ),
    (CommitLabel::TURN_ADMIT, Coverage::Cases(&[Case::Turn])),
    (
        CommitLabel::TURN_PREPARE,
        Coverage::Unemitted(
            "the phase runner commits a turn's prepared context with its first model.start (L3, FIG-5172)",
        ),
    ),
    (CommitLabel::MODEL_START, Coverage::Cases(&[Case::Turn])),
    (
        CommitLabel::MODEL_DONE,
        Coverage::Cases(&[Case::Round, Case::Cancel]),
    ),
    (
        CommitLabel::ROUND_PRESENT_MODEL_START,
        Coverage::Cases(&[Case::Round]),
    ),
    (
        CommitLabel::COMPLETION_START,
        Coverage::Cases(&[Case::Compaction, Case::Pressure]),
    ),
    (
        CommitLabel::PRESSURE_FRAME,
        Coverage::Cases(&[Case::Pressure]),
    ),
    (CommitLabel::TURN_COMMIT, Coverage::Cases(&[Case::Turn])),
    (CommitLabel::TURN_CANCEL, Coverage::Cases(&[Case::Cancel])),
    (CommitLabel::SESSION_RELEASE, Coverage::Cases(&[Case::Turn])),
    (
        CommitLabel::SESSION_COMMAND,
        Coverage::Cases(&[Case::Command]),
    ),
    (
        CommitLabel::ROUND_OUTCOME,
        Coverage::Cases(&[Case::Round, Case::Effects, Case::Cell]),
    ),
    (CommitLabel::ROUND_RETRY, Coverage::Cases(&[Case::Round])),
    (CommitLabel::ROUND_START, Coverage::Cases(&[Case::Round])),
    (CommitLabel::ROUND_TRACED, Coverage::Cases(&[Case::Round])),
    (
        CommitLabel::TOOL_EFFECT,
        Coverage::Unemitted(
            "only a code cell's tool call that stages a store-local effect commits under it (`ActorContext::commit_store_local`, reached from `run_call`), until FIG-5225 admits a cell's calls as round members; no case's cell stages one",
        ),
    ),
    (CommitLabel::WAIT_MINT, Coverage::Cases(&[Case::Close])),
    (CommitLabel::WAIT_RESOLVE, Coverage::Cases(&[Case::Process])),
    (CommitLabel::WAIT_TIMEOUT, Coverage::Cases(&[Case::Process])),
    (
        CommitLabel::WAIT_REVOKE,
        Coverage::Unemitted(
            "its one committer, `ActorContext::retire_closed_run_waits`, is reached only from ADR 0109's scope-close obligation relay (lash-core runtime/obligations/scope_close.rs), which no durable activation constructs; a close revokes under session.close.revoke",
        ),
    ),
    (
        CommitLabel::PROCESS_REGISTER,
        Coverage::Unemitted(
            "a registration creates its process actor through the registry port's own transaction (L6, FIG-5175)",
        ),
    ),
    (
        CommitLabel::PROCESS_ADVANCE,
        Coverage::Cases(&[Case::Process]),
    ),
    (
        CommitLabel::STEP_START,
        Coverage::Unemitted(
            "a step's admission and start commit with the transition that requests it, under process.advance (L6, FIG-5175)",
        ),
    ),
    (CommitLabel::STEP_OUTCOME, Coverage::Cases(&[Case::Process])),
    (
        CommitLabel::PROCESS_CANCEL,
        Coverage::Cases(&[Case::Cancel]),
    ),
    (
        CommitLabel::PROCESS_TERMINAL,
        Coverage::Cases(&[Case::Process]),
    ),
    (
        CommitLabel::CASCADE_BATCH,
        Coverage::Cases(&[Case::Process]),
    ),
    (
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        Coverage::Cases(&[Case::Cell]),
    ),
    (
        CommitLabel::CELL_INJECT,
        Coverage::Cases(&[Case::CellKilled]),
    ),
    (CommitLabel::CELL_SNAPSHOT, Coverage::Cases(&[Case::Cell])),
    (
        CommitLabel::SESSION_CLOSE_BEGIN,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::SESSION_CLOSE_CANCEL,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::SESSION_CLOSE_REVOKE,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::SESSION_CLOSE_END_SCOPE,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::SESSION_CLOSE_ARTIFACTS,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::SESSION_CLOSE_TOMBSTONE,
        Coverage::Cases(&[Case::Close]),
    ),
    (
        CommitLabel::MAIL_SESSION,
        Coverage::Cases(&[Case::Cancel, Case::Close]),
    ),
    (CommitLabel::MAIL_PROCESS, Coverage::Cases(&[Case::Cancel])),
    (CommitLabel::DRAIN_RELEASE, Coverage::Cases(&[Case::Drain])),
];

/// What the audit found over the uncut runs: the violations, and the share
/// of emitted labels the matrix cuts.
#[derive(Clone, Debug, PartialEq)]
pub struct Audit {
    pub violations: Vec<String>,
    /// Labels some case commits, so the matrix cuts.
    pub cut: usize,
    /// Labels the catalog lists as emitted.
    pub emitted: usize,
}

impl Audit {
    /// The share of emitted labels the matrix cuts, in percent.
    #[must_use]
    pub fn coverage_percent(&self) -> f64 {
        if self.emitted == 0 {
            return 0.0;
        }
        #[expect(clippy::cast_precision_loss, reason = "label counts are a few dozen")]
        let share = self.cut as f64 / self.emitted as f64;
        share * 100.0
    }
}

/// Audit [`COVERAGE`] against the labels each case's uncut run committed.
#[must_use]
pub fn audit(committed: &BTreeMap<Case, Vec<CommitLabel>>) -> Audit {
    let mut violations = Vec::new();
    let mut rows: BTreeMap<CommitLabel, Coverage> = BTreeMap::new();
    for (label, coverage) in COVERAGE {
        if !CommitLabel::ALL.contains(&label) {
            violations.push(format!(
                "{label} has a coverage row but is not in the catalog"
            ));
        }
        if rows.insert(label, coverage).is_some() {
            violations.push(format!("{label} has two coverage rows"));
        }
    }
    for label in CommitLabel::ALL {
        if !rows.contains_key(&label) {
            violations.push(format!("{label} is in the catalog but has no coverage row"));
        }
    }
    let committed_by = |label: CommitLabel| -> Vec<Case> {
        committed
            .iter()
            .filter(|(_, labels)| labels.contains(&label))
            .map(|(case, _)| *case)
            .collect()
    };
    let mut cut = 0;
    let mut emitted = 0;
    for (label, coverage) in &rows {
        let by = committed_by(*label);
        match coverage {
            Coverage::Cases(cases) => {
                emitted += 1;
                let missing: Vec<&str> = cases
                    .iter()
                    .filter(|case| committed.contains_key(case) && !by.contains(case))
                    .map(|case| case.name())
                    .collect();
                if !missing.is_empty() {
                    violations.push(format!(
                        "{label} is covered by {} but their uncut runs never committed it",
                        missing.join(", ")
                    ));
                }
                if !by.is_empty() {
                    cut += 1;
                }
            }
            Coverage::Unemitted(why) if !by.is_empty() => violations.push(format!(
                "{label} is listed as unemitted ({why}), but {} committed it",
                by.iter()
                    .map(|case| case.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Coverage::Unemitted(_) => {}
        }
    }
    Audit {
        violations,
        cut,
        emitted,
    }
}
