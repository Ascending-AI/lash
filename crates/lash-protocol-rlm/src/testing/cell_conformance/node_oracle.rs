//! The Node session oracle (FIG-3599): multi-cell sessions against a real
//! engine.
//!
//! The corpus and its reference answers live with the dialect, in
//! `crates/lash-typescript/tests/differential/sessions/`: `corpus.txt` holds
//! the sessions, and `generate.mjs` runs them under the pinned Node (v25.2.1,
//! the version the expression oracle pins), each cell a successive classic
//! Script in one realm, into the checked-in `expectations.json`. This module
//! runs every session through the production RLM executor, once with one live
//! state and once restarting through the durable snapshot path between every
//! pair of cells, and requires each cell to observe what Node observed.
//!
//! The mapping from a cell to its Script is stated once, in the corpus
//! README and ADR 0062: printed lines under the host printer of register
//! entry 13, `finish(value)` as the end of the cell with that value, an
//! uncaught error by its class, no completion value (a cell surfaces none), a
//! statically rejected cell never entering the realm, and after every cell a
//! binding-visibility probe per session binder name,
//! `console.log(typeof NAME, JSON.stringify(NAME))`, where a ReferenceError
//! (or the dialect's static unknown-binding rejection, its exact
//! counterpart) answers `unbound`.
//!
//! A divergence is never a special case here. A cell the dialect rejects
//! says so (`cell reject TS_*`) and must reject with exactly that code. A cell
//! that deviates names its register entry and states the lash answer, which
//! must differ from Node's (the ratchet) and ends its session. A session-wide
//! probe rule names its register entry too; the only one is
//! `closure-boundary`, under which a binding whose value reaches a function
//! answers `unbound` after its cell (ADR 0076).
//!
//! After every cell the session's live globals must also be names the
//! session's own probes answer as bound: a generated slot or a block binding
//! that reached the session (the FIG-3571 phase-1 leaks) fails here even
//! when no probe names it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::harness::{HarnessMode, Session};

const EXPECTATIONS: &str =
    include_str!("../../../../lash-typescript/tests/differential/sessions/expectations.json");

/// The session-wide probe rule of register entry `closure-boundary`.
const CLOSURE_BOUNDARY: &str = "closure-boundary";

#[derive(Deserialize)]
struct Corpus {
    node: String,
    sessions: Vec<SessionCase>,
}

#[derive(Deserialize)]
struct SessionCase {
    id: String,
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

/// What one cell observably did, in the one shape both engines are reduced to.
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Observation {
    /// `normal`, `finish`, `throw` or `rejected`.
    outcome: String,
    /// The class of the uncaught error, for `throw`.
    #[serde(default)]
    error: Option<String>,
    /// The value the cell finished with, for `finish`.
    #[serde(default)]
    finish: Option<serde_json::Value>,
    /// The diagnostic a `rejected` cell names.
    #[serde(default)]
    diagnostic: Option<String>,
    prints: Vec<String>,
    probes: BTreeMap<String, String>,
    /// Lash only: the failure's own text, reported with a mismatch and not
    /// itself compared.
    #[serde(skip)]
    detail: Option<String>,
    /// Node only: the probed names whose value reaches a function. It feeds
    /// the `closure-boundary` rule and is not itself compared.
    #[serde(default)]
    closures: Vec<String>,
}

impl Observation {
    /// The observation without what is reported but never compared, and
    /// with every content hash (a 64-digit hex run) spelled `<hash>`: a hash
    /// is an identity the artifact derives, not something a cell observes.
    fn comparable(&self) -> Self {
        Self {
            detail: None,
            closures: Vec::new(),
            prints: self
                .prints
                .iter()
                .map(|text| without_hashes(text))
                .collect(),
            probes: self
                .probes
                .iter()
                .map(|(name, answer)| (name.clone(), without_hashes(answer)))
                .collect(),
            ..self.clone()
        }
    }
}

fn without_hashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() == 64 {
            out.push_str("<hash>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for character in text.chars() {
        if character.is_ascii_hexdigit() && !character.is_ascii_uppercase() {
            run.push(character);
        } else {
            flush(&mut run, &mut out);
            out.push(character);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn corpus() -> Corpus {
    let mut corpus: Corpus =
        serde_json::from_str(EXPECTATIONS).expect("the session expectations parse");
    for cell in corpus
        .sessions
        .iter_mut()
        .flat_map(|session| session.cells.iter_mut())
    {
        let answers = cell.lash.as_mut().into_iter().flat_map(|answers| {
            answers
                .resident
                .iter_mut()
                .chain(answers.restart.iter_mut())
        });
        for observation in std::iter::once(&mut cell.node).chain(answers) {
            observation.finish = observation.finish.as_ref().map(numbers_as_floats);
        }
    }
    assert_eq!(
        corpus.node, "v25.2.1",
        "the session oracle pins the expression oracle's Node"
    );
    corpus
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
            expected.probes.insert(name.clone(), "unbound".to_string());
        }
    }
    expected
}

/// The diagnostic the dialect statically rejects `source` with, linked
/// exactly as the executor links a cell: against the session's live globals.
fn static_rejection(session: &Session, source: &str) -> Option<String> {
    let mut globals = session.global_names();
    globals.insert("history".to_string());
    let environment = lash_lashlang_runtime::LashlangSurface::default()
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the default surface builds a host environment")
        .with_globals(globals);
    lash_typescript::link(source, &environment)
        .err()
        .map(|diagnostic| diagnostic.code.as_str().to_string())
}

/// The class of an uncaught failure. An uncaught thrown value reaches the
/// host detached, as `{ name, message }` for an Error, and its class is that
/// `name`. Any other failure is a fault the VM raised, which a `catch` would
/// have received as the substrate's `RuntimeError` brand (ADR 0062, register
/// entry `runtime-fault-brand`), so that is its class.
fn thrown_class(message: &str) -> String {
    message
        .strip_prefix("uncaught lashlang exception: ")
        .and_then(|rest| rest.lines().next())
        .and_then(|thrown| serde_json::from_str::<serde_json::Value>(thrown).ok())
        .map_or_else(
            || "RuntimeError".to_string(),
            |thrown| match thrown {
                serde_json::Value::Object(fields) => fields
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Object")
                    .to_string(),
                serde_json::Value::String(_) => "string".to_string(),
                serde_json::Value::Number(_) => "number".to_string(),
                serde_json::Value::Bool(_) => "boolean".to_string(),
                serde_json::Value::Null => "object".to_string(),
                serde_json::Value::Array(_) => "Array".to_string(),
            },
        )
}

/// JSON with every number as a float: the two engines spell an integral
/// number differently (`42` and `42.0`), and that spelling is not an
/// observation.
fn numbers_as_floats(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(number) => number
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        serde_json::Value::Array(items) => items.iter().map(numbers_as_floats).collect(),
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, item)| (key.clone(), numbers_as_floats(item)))
            .collect(),
        other => other.clone(),
    }
}

fn observe_cell(session: &mut Session, source: &str) -> Observation {
    let rejection = static_rejection(session, source);
    let response = session.run_observed(source);
    let mut observation = Observation {
        outcome: "normal".to_string(),
        error: None,
        finish: None,
        diagnostic: None,
        prints: response
            .observations
            .iter()
            .map(|observation| observation.text.clone())
            .collect(),
        probes: BTreeMap::new(),
        detail: response.error.as_ref().map(|failed| failed.message.clone()),
        closures: Vec::new(),
    };
    match (rejection, &response.error, &response.terminal_finish) {
        (Some(code), Some(_), _) => {
            observation.outcome = "rejected".to_string();
            observation.diagnostic = Some(code);
        }
        (Some(code), None, _) => {
            observation.outcome = "linked-rejection-but-ran".to_string();
            observation.diagnostic = Some(code);
        }
        (None, Some(failed), _) => {
            observation.outcome = "throw".to_string();
            observation.error = Some(thrown_class(&failed.message));
        }
        (None, None, Some(value)) => {
            observation.outcome = "finish".to_string();
            observation.finish = Some(numbers_as_floats(value));
        }
        (None, None, None) => {}
    }
    observation
}

/// A probe answers `unbound` exactly where Node's ReferenceError does: the
/// dialect refuses an unbound name statically (`TS_UNKNOWN_BINDING`), its
/// counterpart of the runtime `ReferenceError` (ADR 0062).
fn probe(session: &mut Session, name: &str) -> String {
    let source = format!("console.log(typeof {name}, JSON.stringify({name}));");
    let rejection = static_rejection(session, &source);
    let response = session.run_observed(&source);
    match (rejection.as_deref(), &response.error) {
        (Some("TS_UNKNOWN_BINDING"), Some(_)) => "unbound".to_string(),
        (None, None) => response
            .observations
            .iter()
            .map(|observation| observation.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        (rejection, failed) => format!(
            "probe failed: {rejection:?} {:?}",
            failed.as_ref().map(|failed| failed.message.as_str())
        ),
    }
}

#[test]
fn every_session_observes_what_node_observes() {
    let corpus = corpus();
    let mut failures = Vec::new();
    for mode in HarnessMode::ALL {
        for case in &corpus.sessions {
            let mut session = Session::open(*mode);
            for (index, cell) in case.cells.iter().enumerate() {
                let context = format!("{mode:?} session `{}` cell {index}", case.id);
                let mut actual = observe_cell(&mut session, &cell.source);
                for name in &case.probe {
                    actual
                        .probes
                        .insert(name.clone(), probe(&mut session, name));
                }
                let globals = session.global_names();
                let bound = actual
                    .probes
                    .iter()
                    .filter(|(_, answer)| answer.as_str() != "unbound")
                    .map(|(name, _)| name.clone())
                    .collect::<BTreeSet<_>>();
                let stray = globals.difference(&bound).cloned().collect::<Vec<_>>();
                if !stray.is_empty() {
                    failures.push(format!(
                        "{context}: session globals no probe answers as bound: {stray:?}"
                    ));
                }
                let expected = expected(case, cell, *mode);
                if actual.comparable() != expected.comparable() {
                    failures.push(format!(
                        "{context}:\n  source: {:?}\n  lash:   {actual:?}\n  expect: {expected:?}\n  node:   {:?}",
                        cell.source, cell.node
                    ));
                }
                let stated = cell.lash.as_ref().and_then(|answers| answers.get(*mode));
                if stated.is_some() && actual.comparable() == cell.node.comparable() {
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
