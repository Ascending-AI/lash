//! Shrinks a generated divergence to the session-corpus row that pins it.
//!
//! A generated session that departs from Node is evidence, not a corpus row:
//! it is long, and most of it is irrelevant. The minimizer deletes what it
//! can while the divergence stays the same one (the same
//! [`Divergence::signature`] in the same harness mode, at the session's last
//! cell): first every cell after the diverging one, then whole earlier cells,
//! then top-level statements, to a fixed point. It asks the pinned Node live
//! for every candidate, so a candidate Node rejects (a statement another one
//! needed) is simply not smaller.
//!
//! The result is printed in `corpus.txt`'s format with lash's answer stated,
//! ready for its `about` line and its register or open-defect name.

use std::collections::BTreeSet;

use super::super::harness::HarnessMode;
use super::generated::{Divergence, first_divergence, node_answers};
use super::generator::{GeneratedCell, GeneratedSession};
use super::node::NodeOracle;

/// Whether `candidate` still diverges from Node as `signature` does, at its
/// last cell.
fn still_diverges(
    oracle: &mut NodeOracle,
    candidate: &GeneratedSession,
    mode: HarnessMode,
    signature: &str,
) -> bool {
    let answers = node_answers(oracle, candidate);
    first_divergence(candidate, &answers, mode).is_some_and(|divergence| {
        divergence.cell + 1 == candidate.cells.len() && divergence.signature() == signature
    })
}

/// The binders a session still declares: the probed names its sources
/// mention.
fn with_probes_it_needs(mut session: GeneratedSession, probe: &[String]) -> GeneratedSession {
    let text = session
        .cells
        .iter()
        .map(GeneratedCell::source)
        .collect::<String>();
    session.probe = probe
        .iter()
        .filter(|name| super::generator::mentions(&text, name))
        .cloned()
        .collect();
    session
}

/// Whether one line of a statement is a statement of its own, which a
/// candidate may delete: it ends its statement and opens or closes nothing.
fn is_whole_statement(line: &str) -> bool {
    let line = line.trim();
    let balance = |open: char, close: char| {
        line.chars().filter(|character| *character == open).count()
            == line.chars().filter(|character| *character == close).count()
    };
    line.ends_with(';') && balance('{', '}') && balance('(', ')') && balance('[', ']')
}

/// The smallest session showing `divergence`, rendered as a corpus row.
pub(super) fn minimize(
    oracle: &mut NodeOracle,
    seed: u64,
    session: &GeneratedSession,
    mode: HarnessMode,
    divergence: &Divergence,
) -> String {
    let signature = divergence.signature();
    let probe = session.probe.clone();
    let mut best = session.clone();
    best.cells.truncate(divergence.cell + 1);
    let mut changed = true;
    while changed {
        changed = false;
        // Whole cells before the diverging one.
        let mut index = best.cells.len().saturating_sub(1);
        while index > 0 {
            index -= 1;
            let mut candidate = best.clone();
            candidate.cells.remove(index);
            if still_diverges(oracle, &candidate, mode, &signature) {
                best = candidate;
                changed = true;
            }
        }
        // Top-level statements, last first.
        for cell in (0..best.cells.len()).rev() {
            let mut statement = best.cells[cell].statements.len();
            while statement > 0 {
                statement -= 1;
                if best.cells[cell].statements.len() == 1 {
                    break;
                }
                let mut candidate = best.clone();
                candidate.cells[cell].statements.remove(statement);
                if still_diverges(oracle, &candidate, mode, &signature) {
                    best = candidate;
                    changed = true;
                }
            }
        }
        // Lines inside a statement that are statements of their own.
        for cell in (0..best.cells.len()).rev() {
            for statement in (0..best.cells[cell].statements.len()).rev() {
                let mut line = best.cells[cell].statements[statement].lines().count();
                while line > 0 {
                    line -= 1;
                    let lines = best.cells[cell].statements[statement]
                        .lines()
                        .collect::<Vec<_>>();
                    let Some(text) = lines.get(line) else {
                        continue;
                    };
                    if lines.len() < 2 || !is_whole_statement(text) {
                        continue;
                    }
                    let mut candidate = best.clone();
                    candidate.cells[cell].statements[statement] = lines
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != line)
                        .map(|(_, text)| *text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if still_diverges(oracle, &candidate, mode, &signature) {
                        best = candidate;
                        changed = true;
                    }
                }
            }
        }
        // Probes of names the session no longer mentions.
        let trimmed = with_probes_it_needs(best.clone(), &probe);
        if trimmed.probe != best.probe && still_diverges(oracle, &trimmed, mode, &signature) {
            best = trimmed;
            changed = true;
        }
    }
    let answers = node_answers(oracle, &best);
    let last = first_divergence(&best, &answers, mode)
        .expect("the minimized session keeps its divergence");
    corpus_row(seed, &best, &answers, mode, &last)
}

/// `session` as a `corpus.txt` row whose last cell states lash's answer in
/// `mode`, with its register or open-defect name left to write.
fn corpus_row(
    seed: u64,
    session: &GeneratedSession,
    answers: &[super::Observation],
    mode: HarnessMode,
    divergence: &Divergence,
) -> String {
    let mut row = vec![
        format!("session generated-{seed}"),
        "about <what the session shows>".to_string(),
        format!("probe {}", session.probe.join(" ")),
    ];
    let closures = answers
        .iter()
        .flat_map(|answer| answer.closures.iter())
        .collect::<BTreeSet<_>>();
    if !closures.is_empty() {
        row.push(format!("deviation {}", super::CLOSURE_BOUNDARY));
    }
    for (index, cell) in session.cells.iter().enumerate() {
        let head = match (&cell.reject, index + 1 == session.cells.len()) {
            (Some(code), _) => format!("cell reject {code}"),
            (None, true) => "cell defect <open-defect or deviation name>".to_string(),
            (None, false) => "cell".to_string(),
        };
        row.push(head);
        row.push(cell.source().trim_end().to_string());
    }
    let keyword = match mode {
        HarnessMode::Resident => "lash-resident",
        HarnessMode::RestartBetweenCells => "lash-restart",
    };
    row.push(format!(
        "{keyword} {}",
        serde_json::to_string(&divergence.lash).expect("an observation serializes")
    ));
    row.push("end".to_string());
    row.join("\n")
}
