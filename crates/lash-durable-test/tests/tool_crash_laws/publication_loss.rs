//! L1(f), D4: container publication is at most once, owned by the owner
//! whose acknowledged commit wrote the container's terminal (§3.8, P1). A
//! cut that lands a terminal commit and then loses its owner's
//! acknowledgement or its life before the container's publication loses
//! that container, and its children already published keep a parent id no
//! span carries. No other cut loses anything.
//!
//! The matrix measures the window: for every terminal label it cut, under
//! every fault, it reports what the cut lost and the orphans it left.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash::tracing::TraceSpanKind;
use lash_durable::CommitLabel;
use lash_durable_test::{Cut, Fault};
use lash_sansio::sync::MutexExt as _;

use super::telemetry::{Lost, Telemetry};

/// The commits that write a container's terminal, and the kinds of the
/// containers each one closes, for the containers the matrix's turns
/// publish (§3.8's closer table): a turn's commit closes the turn, a
/// round's outcome commit closes the tool calls whose outcomes it retains,
/// and the commit that consumes a code cell's answer (the next model call's
/// start, the next step's `model.done`, or the turn's commit) closes the
/// cell.
const TERMINALS: [(CommitLabel, &[TraceSpanKind]); 6] = [
    (
        CommitLabel::TURN_COMMIT,
        &[TraceSpanKind::Agent, TraceSpanKind::Cell],
    ),
    (CommitLabel::ROUND_OUTCOME, &[TraceSpanKind::Tool]),
    (
        CommitLabel::ROUND_OUTCOME_PRESENT_MODEL_START,
        &[TraceSpanKind::Tool, TraceSpanKind::Cell],
    ),
    (CommitLabel::MODEL_START, &[TraceSpanKind::Cell]),
    (
        CommitLabel::ROUND_PRESENT_MODEL_START,
        &[TraceSpanKind::Cell],
    ),
    (CommitLabel::MODEL_DONE, &[TraceSpanKind::Cell]),
];

/// The faults that land a commit and lose its owner's acknowledgement or
/// its life before the owner publishes what the commit closed.
const LOSSY: [Fault; 2] = [Fault::AckHidden, Fault::CommitThenAbort];

/// The container kinds a run cut at `cut` may lose: the ones its commit
/// closes, when the cut lands it and loses its acknowledgement or its
/// owner (D4). Every other cut loses nothing.
fn losable(cut: Option<&Cut>) -> &'static [TraceSpanKind] {
    let Some(cut) = cut.filter(|cut| LOSSY.contains(&cut.fault)) else {
        return &[];
    };
    TERMINALS
        .iter()
        .find(|(label, _)| *label == cut.point.label)
        .map_or(&[], |(_, kinds)| kinds)
}

/// What the runs cut at one point under one fault lost, rendered.
type Measured = BTreeMap<(String, Fault), BTreeSet<String>>;

/// What each cut of one matrix lost, shared by every cell's scenario.
#[derive(Clone, Default)]
pub(super) struct Losses(Arc<Mutex<Measured>>);

impl Losses {
    /// The trace laws of one run cut at `cut`, recording what it lost:
    /// every container closes once and every published parent was
    /// published (L1(c), (g)), except that a cut that lands a terminal
    /// commit and loses its owner's acknowledgement or life may lose the
    /// containers that commit closes, and only those (L1(f)). `tools` says
    /// whether the turn makes a tool call, and `cells` whether it calls it
    /// from a code cell, which is then one span, the call's parent (T5).
    pub(super) fn laws(
        &self,
        telemetry: &Telemetry,
        cut: Option<&Cut>,
        tools: bool,
        cells: bool,
    ) -> Vec<String> {
        let losable = losable(cut);
        if let Some(cut) = cut {
            self.record(cut, &telemetry.lost());
        }
        let turn_lost = losable.contains(&TraceSpanKind::Agent);
        let mut violations = telemetry.first_turn_violations(super::SESSION, turn_lost);
        violations.extend(telemetry.publication_violations(losable));
        if tools {
            let tool_lost = losable.contains(&TraceSpanKind::Tool);
            violations.extend(telemetry.first_turn_tool_violations(super::SESSION, tool_lost));
        }
        if cells {
            let cell_lost = losable.contains(&TraceSpanKind::Cell);
            violations.extend(telemetry.first_turn_cell_violations(super::SESSION, cell_lost));
        }
        violations
    }

    /// Record what the run cut at `cut` lost.
    pub(super) fn record(&self, cut: &Cut, lost: &[Lost]) {
        let rendered = if lost.is_empty() {
            BTreeSet::from(["nothing".to_owned()])
        } else {
            lost.iter().map(ToString::to_string).collect()
        };
        self.0
            .lock_recover()
            .entry((cut.point.to_string(), cut.fault))
            .or_default()
            .extend(rendered);
    }

    /// Report, for every terminal label the matrix cut, what each fault's
    /// cut lost and the orphans it left (L1(f) per label), and fail unless
    /// every such cut under each lossy fault in `faults` was measured.
    pub(super) fn report(&self, scenario: &str, cut: &[CommitLabel], faults: &[Fault]) {
        let measured = self.0.lock_recover();
        let mut lines = Vec::new();
        let mut unmeasured = Vec::new();
        for (label, _) in TERMINALS.iter().filter(|(label, _)| cut.contains(label)) {
            let rows: Vec<_> = measured
                .iter()
                .filter(|((point, _), _)| {
                    point
                        .rsplit_once('#')
                        .is_some_and(|(at, _)| at == label.as_str())
                })
                .collect();
            for fault in faults.iter().filter(|fault| LOSSY.contains(fault)) {
                if !rows.iter().any(|((_, measured), _)| measured == fault) {
                    unmeasured.push(format!("{label} {fault}"));
                }
            }
            for ((point, fault), lost) in rows {
                let lost: Vec<_> = lost.iter().map(String::as_str).collect();
                lines.push(format!("{point} {fault}: loses {}", lost.join("; ")));
            }
        }
        eprintln!(
            "publication loss (D4) of {scenario}, per terminal label:\n  {}",
            lines.join("\n  ")
        );
        assert!(
            unmeasured.is_empty(),
            "{scenario}: no cut measured the publication loss of {unmeasured:?}"
        );
    }
}
