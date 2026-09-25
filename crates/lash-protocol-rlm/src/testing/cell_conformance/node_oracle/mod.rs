//! The Node session oracle: multi-cell sessions against a real engine.
//!
//! Two corpora of sessions live with the dialect, in
//! `crates/lash-typescript/tests/differential/sessions/`, and both are
//! answered by the pinned Node (v25.2.1, the version the expression oracle
//! pins) under one cell-to-Script mapping (`realm.mjs`): each cell a
//! successive classic Script in one realm.
//!
//! * [`corpus`] (FIG-3599): the hand-written sessions of `corpus/`, whose
//!   answers `generate.mjs` checks in under `expectations/`.
//! * [`generated`] (FIG-3608): sessions drawn by the seeded [`generator`]
//!   from the dialect's accepted grammar, and the rows of the snapshot
//!   [`round_trip`] law, whose answers the [`node`] oracle service checks in
//!   under `generated/`. Longer runs ask Node live, and every divergence is
//!   [`minimize`]d into a corpus row.
//!
//! This module runs a session through the production RLM executor, once
//! with one live state and once restarting through the durable snapshot path
//! between every pair of cells, and reduces each cell to the one
//! [`Observation`] both engines are compared in.
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
//! A divergence is never a special case. A cell the dialect rejects says so
//! and must reject with exactly that code. A divergence names its register
//! entry or open defect, and a named divergence that closes fails until its
//! name is deleted (the ratchet). The session-wide probe rule of register
//! entry `closure-boundary` turns a binding whose value reaches a function
//! into `dropped` after its cell (ADR 0076): a later cell's reference to it is
//! refused by name.
//!
//! After every cell the session's live globals must also be names the
//! session's own probes answer as bound: a generated slot or a block binding
//! that reached the session (the FIG-3571 phase-1 leaks) fails here even
//! when no probe names it.

mod corpus;
mod generated;
mod generator;
mod minimize;
mod node;
mod round_trip;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::harness::{HarnessMode, Session};

/// The Node every answer is pinned to: the expression oracle's.
const PINNED_NODE: &str = "v25.2.1";

/// The session-wide probe rule of register entry `closure-boundary`.
const CLOSURE_BOUNDARY: &str = "closure-boundary";

/// A probe's answer for a name the `closure-boundary` rule dropped: a later
/// cell's reference to it is refused by name (`TS_FUNCTION_NOT_PERSISTED`).
const DROPPED: &str = "dropped";

/// What one cell observably did, in the one shape both engines are reduced to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Observation {
    /// `normal`, `finish`, `throw` or `rejected`.
    outcome: String,
    /// The class of the uncaught error, for `throw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The value the cell finished with, for `finish`. `finish(null)` is
    /// present and `null`, which is not the absence of a finish.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    finish: Option<serde_json::Value>,
    /// The diagnostic a `rejected` cell names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
    prints: Vec<String>,
    probes: BTreeMap<String, String>,
    /// Lash only: the failure's own text, reported with a mismatch and not
    /// itself compared.
    #[serde(skip)]
    detail: Option<String>,
    /// Node only: the probed names whose value reaches a function. It feeds
    /// the `closure-boundary` rule and is not itself compared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
            finish: self.finish.as_ref().map(numbers_as_floats),
            ..self.clone()
        }
    }

    /// Whether two observations agree on everything a cell observes.
    fn agrees_with(&self, other: &Self) -> bool {
        self.comparable() == other.comparable()
    }
}

/// A field that is present is `Some`, whatever it holds: a missing one is the
/// field's default, `None`.
fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
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

/// The diagnostic the dialect statically rejects `source` with, linked
/// exactly as the executor links a cell: against the session's live globals.
/// The diagnostic's message is returned beside its code.
fn static_rejection(session: &Session, source: &str) -> Option<(String, String)> {
    let mut globals = session.global_names();
    globals.extend(session.host_binding_names());
    link_rejection(source, globals, session.expired_functions())
}

/// The diagnostic `source` is refused with when linked against `globals`, and
/// the `expired` functions a cell boundary dropped, on the default surface,
/// as the executor links a cell.
fn link_rejection(
    source: &str,
    mut globals: BTreeSet<String>,
    expired: BTreeSet<String>,
) -> Option<(String, String)> {
    globals.insert("history".to_string());
    let environment = lash_lashlang_runtime::LashlangSurface::default()
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the default surface builds a host environment")
        .with_globals(globals)
        .with_expired_functions(expired);
    lash_typescript::link(source, &environment)
        .err()
        .map(|diagnostic| {
            (
                diagnostic.code.as_str().to_string(),
                diagnostic.message.clone(),
            )
        })
}

/// The class of an uncaught failure. An uncaught thrown value reaches the
/// host detached, as `{ name, message }` for an Error, and its class is that
/// `name` — which is also how a fault in an operation ECMA-262 specifies to
/// throw arrives, as the `TypeError` (or other class) the VM threw in its
/// place. Any other failure is a fault with no ECMA counterpart, which a
/// `catch` would have received as the substrate's `RuntimeError` brand
/// (ADR 0062, register entry `runtime-fault-brand`), so that is its class.
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
        (Some((code, message)), Some(_), _) => {
            observation.outcome = "rejected".to_string();
            observation.diagnostic = Some(code);
            observation.detail = Some(message);
        }
        (Some((code, _)), None, _) => {
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

/// The probe of `name`: `console.log(typeof NAME, JSON.stringify(NAME))`.
fn probe_source(name: &str) -> String {
    format!("console.log(typeof {name}, JSON.stringify({name}));")
}

/// Every probe of `names`, each as its own cell would answer it.
///
/// A probe answers `unbound` exactly where Node's ReferenceError does: the
/// dialect refuses an unbound name statically (`TS_UNKNOWN_BINDING`), its
/// counterpart of the runtime `ReferenceError` (ADR 0062). A name a cell
/// boundary dropped for holding a function answers `dropped`: the dialect
/// refuses it by name (`TS_FUNCTION_NOT_PERSISTED`). The probes of the
/// bound names read and never write, so they run as one cell, whose lines
/// are theirs in order; if that cell does not print one line per probe, each
/// probe runs as its own cell instead.
fn probes(session: &mut Session, names: &[String]) -> BTreeMap<String, String> {
    let mut answers = BTreeMap::new();
    let mut bound = Vec::new();
    for name in names {
        match static_rejection(session, &probe_source(name)) {
            Some((code, _)) if code == "TS_UNKNOWN_BINDING" => {
                answers.insert(name.clone(), "unbound".to_string());
            }
            Some((code, _)) if code == "TS_FUNCTION_NOT_PERSISTED" => {
                answers.insert(name.clone(), DROPPED.to_string());
            }
            Some(rejection) => {
                answers.insert(name.clone(), format!("probe failed: {rejection:?}"));
            }
            None => bound.push(name.clone()),
        }
    }
    if bound.is_empty() {
        return answers;
    }
    let batch = bound
        .iter()
        .map(|name| probe_source(name))
        .collect::<Vec<_>>()
        .join("\n");
    let response = session.run_observed(&batch);
    if response.error.is_none() && response.observations.len() == bound.len() {
        for (name, line) in bound.into_iter().zip(&response.observations) {
            answers.insert(name, line.text.clone());
        }
        return answers;
    }
    for name in bound {
        let response = session.run_observed(&probe_source(&name));
        let answer = match &response.error {
            None => response
                .observations
                .iter()
                .map(|observation| observation.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            Some(failed) => format!("probe failed: {}", failed.message),
        };
        answers.insert(name, answer);
    }
    answers
}

/// One cell of a session as lash observed it, with the session globals no
/// probe answered as bound: a name that reached the session unprobed.
struct LashCell {
    observation: Observation,
    stray: Vec<String>,
}

/// A session's cells run in `session`, each observed and followed by the
/// probes of `names`.
fn run_cells<'a>(
    session: &mut Session,
    names: &[String],
    sources: impl IntoIterator<Item = &'a str>,
) -> Vec<LashCell> {
    sources
        .into_iter()
        .map(|source| {
            let mut observation = observe_cell(session, source);
            observation.probes = probes(session, names);
            let bound = observation
                .probes
                .iter()
                .filter(|(_, answer)| answer.as_str() != "unbound")
                .map(|(name, _)| name.clone())
                .collect::<BTreeSet<_>>();
            let stray = session.global_names().difference(&bound).cloned().collect();
            LashCell { observation, stray }
        })
        .collect()
}

/// A whole session through lash in one harness mode.
fn run_session(mode: HarnessMode, names: &[String], sources: &[String]) -> Vec<LashCell> {
    let mut session = Session::open(mode);
    run_cells(&mut session, names, sources.iter().map(String::as_str))
}
