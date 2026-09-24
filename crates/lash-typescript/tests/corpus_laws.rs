//! The corpus laws (FIG-3599): laws that hold over every corpus the dialect
//! has, not over one hand-picked script.
//!
//! * [`round_trip`]: print → reparse → admit. Every program, lowered and
//!   admitted, projected and printed through the lens, reparses and admits to
//!   the same `module_ref` and `source_identity` — or the printer refuses it
//!   with a typed error an explicit, ratcheted allowlist names with a reason.
//! * [`invariants`]: the structural invariants of every admitted artifact.
//! * [`sessions`]: the Node session corpus's own discipline.
//!
//! The corpora are listed in [`corpora`]. The multi-cell Node session oracle
//! itself runs in `lash-protocol-rlm`, which owns the RLM executor.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]

use std::collections::BTreeSet;

use lashlang::{Declaration, Expr, Program};

#[path = "corpus_laws/corpora.rs"]
mod corpora;
#[path = "workflow_graph/goldens.rs"]
mod goldens;
#[path = "test262/support/ingest.rs"]
#[allow(
    dead_code,
    reason = "the corpus laws read each test's script and harness bindings, not the runner's one-cell program"
)]
mod ingest;
#[path = "corpus_laws/invariants.rs"]
mod invariants;
// The slice runner reads every metadata field; the corpus laws read the
// negative phase only.
#[path = "test262/support/metadata.rs"]
#[allow(
    dead_code,
    reason = "the corpus laws read a subset of the shared Test262 metadata"
)]
mod metadata;
#[path = "corpus_laws/round_trip.rs"]
mod round_trip;
#[path = "corpus_laws/sessions.rs"]
mod sessions;

/// The identifier namespace the lowerer reserves for its generated bindings
/// (ADR 0062); no authored name can begin with it.
const RESERVED_PREFIX: &str = "__typescript_";

/// Every name `program` binds anywhere: an assignment's root, a loop
/// binding, a function's name and parameters, a catch binding.
fn binders(program: &Program) -> BTreeSet<String> {
    fn walk(expr: &Expr, names: &mut BTreeSet<String>) {
        match expr {
            Expr::Assign { target, .. } if target.is_simple() => {
                names.insert(target.root.to_string());
            }
            Expr::For { binding, .. } => {
                names.insert(binding.to_string());
            }
            Expr::Function(function) => {
                names.extend(function.name.iter().map(ToString::to_string));
                names.extend(function.params.iter().map(ToString::to_string));
            }
            Expr::Try(scope) => {
                if let Some(catch) = &scope.catch {
                    names.insert(catch.binding.to_string());
                }
            }
            _ => {}
        }
        for child in expr.children() {
            walk(child, names);
        }
    }
    let mut names = BTreeSet::new();
    walk(&program.main, &mut names);
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => {
                names.extend(process.params.iter().map(|param| param.name.to_string()));
                walk(&process.body, &mut names);
            }
            Declaration::Function(function) => {
                names.extend(function.params.iter().map(|param| param.name.to_string()));
                walk(&function.body, &mut names);
            }
            Declaration::Type(_) => {}
        }
    }
    names
}

/// Where two JSON documents first differ, for a law violation's report.
fn first_difference(
    left: &serde_json::Value,
    right: &serde_json::Value,
    path: &str,
) -> Option<String> {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => left
            .keys()
            .chain(right.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .find_map(|key| match (left.get(key), right.get(key)) {
                (Some(l), Some(r)) => first_difference(l, r, &format!("{path}.{key}")),
                (l, r) => Some(format!("{path}.{key}: {l:?} vs {r:?}")),
            }),
        (Value::Array(left), Value::Array(right)) => left
            .iter()
            .zip(right)
            .enumerate()
            .find_map(|(index, (l, r))| first_difference(l, r, &format!("{path}[{index}]")))
            .or_else(|| {
                (left.len() != right.len())
                    .then(|| format!("{path}: {} vs {} items", left.len(), right.len()))
            }),
        (left, right) if left != right => {
            let brief = |value: &Value| value.to_string().chars().take(160).collect::<String>();
            Some(format!("{path}: {} vs {}", brief(left), brief(right)))
        }
        _ => None,
    }
}
