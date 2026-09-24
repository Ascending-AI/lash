//! The Node session corpus, read from its generated expectations.
//!
//! The lash side of the session oracle runs in `lash-protocol-rlm`, which
//! owns the RLM executor; this module is the dialect's own view of the same
//! corpus: its cells feed the round-trip law and the artifact invariants, and
//! the corpus discipline (every deviation and defect named where it is
//! registered, every probe rule earning its place, every binder probed) is
//! checked here against the register it names.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

const EXPECTATIONS: &str = include_str!("../differential/sessions/expectations.json");
const README: &str = include_str!("../../README.md");

/// The one session-wide probe rule the register holds.
const CLOSURE_BOUNDARY: &str = "closure-boundary";

#[derive(Deserialize)]
pub(crate) struct Corpus {
    pub(crate) node: String,
    pub(crate) sessions: Vec<Session>,
}

#[derive(Deserialize)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) probe: Vec<String>,
    pub(crate) deviations: Vec<String>,
    pub(crate) cells: Vec<Cell>,
}

#[derive(Deserialize)]
pub(crate) struct Cell {
    pub(crate) source: String,
    pub(crate) reject: Option<String>,
    pub(crate) deviation: Option<String>,
    pub(crate) defect: Option<String>,
    pub(crate) node: Observation,
    #[serde(default)]
    pub(crate) lash: Option<Answers>,
}

/// A deviation or defect cell's lash answers. The dialect's view reads the
/// resident one; the restarting harness mode's is `lash-protocol-rlm`'s.
#[derive(Deserialize)]
pub(crate) struct Answers {
    pub(crate) resident: Option<Observation>,
}

#[derive(Deserialize)]
pub(crate) struct Observation {
    /// `normal`, `finish`, `throw` or `rejected`.
    pub(crate) outcome: String,
    pub(crate) probes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) closures: Vec<String>,
}

pub(crate) fn corpus() -> Corpus {
    serde_json::from_str(EXPECTATIONS).expect("the session expectations parse")
}

impl Session {
    /// The names bound after each cell, as the lash side must answer the
    /// probes of a live (resident) session: Node's answer under the session's
    /// probe rule, or the cell's stated lash answer.
    pub(crate) fn bound_after(&self, cell: &Cell) -> BTreeSet<String> {
        let probes = match cell
            .lash
            .as_ref()
            .and_then(|answers| answers.resident.as_ref())
        {
            Some(answer) => &answer.probes,
            None => &cell.node.probes,
        };
        let dropped = if self.deviations.iter().any(|name| name == CLOSURE_BOUNDARY) {
            cell.node.closures.iter().cloned().collect()
        } else {
            BTreeSet::new()
        };
        probes
            .iter()
            .filter(|(name, answer)| {
                !matches!(answer.as_str(), "unbound" | "tdz") && !dropped.contains(*name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// The register an entry must be in: the crate README's deviation register
/// for a deviation, its open-defect list for a defect.
fn listed(section: &str, name: &str) -> bool {
    let body = README
        .split(section)
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_default();
    body.contains(&format!("`{name}`"))
}

/// The corpus discipline (ADR 0062, extended by FIG-3599): every divergence
/// is named where it is registered, and every named rule earns its place.
#[test]
fn the_session_corpus_names_every_divergence_where_it_is_registered() {
    let corpus = corpus();
    assert_eq!(
        corpus.node, "v25.2.1",
        "the session oracle pins the expression oracle's Node"
    );
    let mut failures = Vec::new();
    for session in &corpus.sessions {
        for deviation in &session.deviations {
            if deviation != CLOSURE_BOUNDARY {
                failures.push(format!(
                    "{}: `{deviation}` is not a session probe rule",
                    session.id
                ));
            }
        }
        // A cell with a stated lash answer states its probes itself.
        let closures = session
            .cells
            .iter()
            .any(|cell| cell.lash.is_none() && !cell.node.closures.is_empty());
        let rule = session
            .deviations
            .iter()
            .any(|name| name == CLOSURE_BOUNDARY);
        if closures != rule {
            failures.push(format!(
                "{}: a session whose bindings reach a function declares `{CLOSURE_BOUNDARY}`, and only such a session",
                session.id
            ));
        }
        for (index, cell) in session.cells.iter().enumerate() {
            let context = format!("{} cell {index}", session.id);
            match (&cell.deviation, &cell.defect) {
                (Some(deviation), None) => {
                    if !listed("## Deviation register", deviation) {
                        failures.push(format!(
                            "{context}: deviation `{deviation}` is not in the README register"
                        ));
                    }
                }
                (None, Some(defect)) => {
                    if !listed("## Open conformance defects", defect) {
                        failures.push(format!(
                            "{context}: defect `{defect}` is not in the README's open-defect list"
                        ));
                    }
                }
                (None, None) => {}
                (Some(_), Some(_)) => failures.push(format!(
                    "{context}: a cell is a deviation or a defect, not both"
                )),
            }
            if (cell.deviation.is_some() || cell.defect.is_some()) != cell.lash.is_some() {
                failures.push(format!(
                    "{context}: a deviation or defect cell states its lash answer"
                ));
            }
        }
    }
    if !listed("## Deviation register", CLOSURE_BOUNDARY) {
        failures.push(format!(
            "`{CLOSURE_BOUNDARY}` is not in the README register"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every binder a session's cells declare is probed: a name that is never
/// probed is a leak nothing would see.
#[test]
fn every_session_binder_is_probed() {
    let corpus = corpus();
    let mut failures = Vec::new();
    for session in &corpus.sessions {
        let probed = session.probe.iter().cloned().collect::<BTreeSet<_>>();
        for cell in &session.cells {
            let Ok(program) = lash_typescript::parse_with_globals(&cell.source, &probed) else {
                continue;
            };
            let unprobed = super::binders(&program)
                .into_iter()
                .filter(|name| !name.starts_with(super::RESERVED_PREFIX))
                .filter(|name| !probed.contains(name))
                .collect::<Vec<_>>();
            if !unprobed.is_empty() {
                failures.push(format!(
                    "{}: binders never probed: {unprobed:?}",
                    session.id
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
