//! The hand-written session corpus (FIG-3599).
//!
//! `corpus/` holds the sessions, one file each, and `generate.mjs` checks in
//! Node's answers under `expectations/`. A cell the dialect rejects says so
//! (`cell reject TS_*`) and must reject with exactly that code. A cell that
//! deviates names its register entry, or a defect its open-defect entry, and
//! states the lash answer, which must differ from Node's (the ratchet) and
//! ends its session.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::super::harness::{HarnessMode, Session};
use super::{CLOSURE_BOUNDARY, Observation, PINNED_NODE, run_cells};

#[derive(Deserialize)]
struct Corpus {
    sessions: Vec<SessionCase>,
}

#[derive(Deserialize)]
struct SessionCase {
    id: String,
    /// The host's read-only projected bindings: plain globals of the Node
    /// realm, lazy projections on the lash side.
    #[serde(default)]
    host: BTreeMap<String, serde_json::Value>,
    probe: Vec<String>,
    deviations: Vec<String>,
    cells: Vec<CellCase>,
}

#[derive(Deserialize)]
struct CellCase {
    source: String,
    reject: Option<String>,
    deviation: Option<String>,
    defect: Option<String>,
    node: Observation,
    /// The lash answer of a deviation or defect cell, per harness mode; a
    /// mode it leaves out answers as Node does.
    #[serde(default)]
    lash: Option<LashAnswers>,
}

#[derive(Deserialize)]
struct LashAnswers {
    resident: Option<Observation>,
    restart: Option<Observation>,
}

impl LashAnswers {
    fn get(&self, mode: HarnessMode) -> Option<&Observation> {
        match mode {
            HarnessMode::Resident => self.resident.as_ref(),
            HarnessMode::RestartBetweenCells => self.restart.as_ref(),
        }
    }
}

/// The session corpus: `expectations/meta.json` for the pinned Node, then
/// every `expectations/<session>.json` shard — one file per session, so two
/// lanes' corpus changes never share one (FIG-3727).
#[allow(clippy::disallowed_methods)] // FIG-2971: a corpus check is a host; the checked-in shards are a test's data.
fn corpus() -> Corpus {
    #[derive(Deserialize)]
    struct Meta {
        node: String,
    }
    let directory = super::node::sessions_directory().join("expectations");
    let meta: Meta = serde_json::from_str(
        &std::fs::read_to_string(directory.join("meta.json")).expect("read expectations/meta.json"),
    )
    .expect("the session expectations' meta parses");
    assert_eq!(
        meta.node, PINNED_NODE,
        "the session oracle pins the expression oracle's Node"
    );
    let mut sessions = super::node::shard_files(&directory, "json")
        .into_iter()
        .filter(|(shard, _)| shard != "meta")
        .map(|(shard, text)| {
            let session: SessionCase = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("expectations/{shard}.json: {error}"));
            assert_eq!(
                session.id, shard,
                "expectations/{shard}.json holds another session"
            );
            session
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    Corpus { sessions }
}

/// The answer Node's observation implies for lash, under the session's
/// registered probe rules.
fn expected(session: &SessionCase, cell: &CellCase, mode: HarnessMode) -> Observation {
    if let Some(lash) = cell.lash.as_ref().and_then(|answers| answers.get(mode)) {
        return lash.clone();
    }
    let mut expected = cell.node.clone();
    expected.closures.clear();
    expected.diagnostic.clone_from(&cell.reject);
    if session
        .deviations
        .iter()
        .any(|name| name == CLOSURE_BOUNDARY)
    {
        for name in &cell.node.closures {
            expected
                .probes
                .insert(name.clone(), super::DROPPED.to_string());
        }
    }
    expected
}

#[test]
fn every_session_observes_what_node_observes() {
    let corpus = corpus();
    let mut failures = Vec::new();
    for mode in HarnessMode::ALL {
        for case in &corpus.sessions {
            let mut session = Session::open_with_host(*mode, &case.host);
            for (index, cell) in case.cells.iter().enumerate() {
                let context = format!("{mode:?} session `{}` cell {index}", case.id);
                let lash = run_cells(&mut session, &case.probe, [cell.source.as_str()])
                    .pop()
                    .expect("one cell ran");
                if !lash.stray.is_empty() {
                    failures.push(format!(
                        "{context}: session globals no probe answers as bound: {:?}",
                        lash.stray
                    ));
                }
                let actual = lash.observation;
                let expected = expected(case, cell, *mode);
                if !actual.agrees_with(&expected) {
                    failures.push(format!(
                        "{context}:\n  source: {:?}\n  lash:   {actual:?}\n  expect: {expected:?}\n  node:   {:?}",
                        cell.source, cell.node
                    ));
                }
                let stated = cell.lash.as_ref().and_then(|answers| answers.get(*mode));
                if stated.is_some() && actual.agrees_with(&cell.node) {
                    failures.push(format!(
                        "{context}: `{}` no longer diverges from Node; delete its lash answer",
                        cell.deviation
                            .as_deref()
                            .or(cell.defect.as_deref())
                            .unwrap_or_default()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
