//! The helper compiler (FIG-6678, FIG-6709): each dialect's helpers,
//! compiled from their kernel bodies to Rust plans the machine runs where a
//! native would answer ([`lash_kernel_doc::plan`]).
//!
//! The bodies stay the definitions. The compiler reads them as a worker
//! registers them, finds which are closed and which reach guest code
//! ([`analysis`]), and emits a plan for each body whose every form it
//! performs as the body does ([`emit`]): a closed body runs to its end, and
//! one that reaches guest code is a resumable segment that yields at each
//! callback site and resumes after it. Every other body runs as it is. The
//! output is checked in beside each dialect's natives, and a test in each
//! of those crates fails when it is not what the compiler gives now.
//!
//! Beside the code, a manifest gives every helper's facts: whether it is
//! closed or what it reaches, through which sites, whether it writes,
//! the closures it makes, its formula, and its plan or why it has none.
//! It is the compiler's artifact, for the lanes that compile the rest; no
//! host reads it.

pub mod analysis;
pub mod emit;
pub mod library;
mod template;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use analysis::Analysis;
use library::{Dialect, Library};

/// The command that writes the checked-in output.
pub const REGENERATE: &str = "kiln run //crates/lash-ext-helper-compiler:regenerate__bin";

/// What the compiler gives for one dialect: the plans' Rust source and the
/// manifest, each as checked in.
pub struct Generated {
    pub rust: String,
    pub manifest: String,
    /// How many helpers have a plan.
    pub planned: usize,
}

/// The Rust item name of the plan of the helper named `name`.
fn ident(name: &str, taken: &mut BTreeSet<String>) -> String {
    let mut ident: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if ident.starts_with(|c: char| c.is_ascii_digit()) {
        ident.insert(0, '_');
    }
    let base = ident.clone();
    let mut suffix = 1;
    while !taken.insert(ident.clone()) {
        suffix += 1;
        ident = format!("{base}_{suffix}");
    }
    ident
}

/// Compiles `dialect`'s helpers.
///
/// # Errors
///
/// A refusal of the helpers' registration, as text.
pub fn generate(dialect: Dialect) -> Result<Generated, String> {
    let library = Library::of(dialect)?;
    let mut generated = generate_library(&library, dialect)?;
    if dialect == Dialect::TypeScript {
        // The fixed table `ts.builtin_name`'s native searches, as the
        // helper's body does.
        generated.rust.push_str(
            "\n/// Every name a computed read finds among the built-in rows, in code\n\
             /// point order: what `ts.builtin_name` searches.\n\
             pub const BUILTIN_NAMES: &[&str] = &[\n",
        );
        for name in lash_dialect_typescript::builtin_names() {
            let _ = writeln!(generated.rust, "    {name:?},");
        }
        generated.rust.push_str("];\n");
    }
    Ok(generated)
}

/// Compiles an admitted library's selected helpers and their closed callees.
/// This is also the recorder seam for the kernel's control-flow laws.
///
/// # Errors
///
/// A refusal to compile a definition, as text.
pub fn generate_library(library: &Library, dialect: Dialect) -> Result<Generated, String> {
    let analysis = Analysis::of(library);
    // The helpers whose plans answer at once: closed ones.
    let mut answering = BTreeSet::new();
    let mut plans = Vec::new();
    let mut outcomes = BTreeMap::new();
    let mut taken = BTreeSet::new();
    // Include closed kernel bodies used by helpers: their natives may decline.
    fn order(
        function: lash_kernel_doc::FunctionId,
        library: &Library,
        analysis: &Analysis,
        seen: &mut BTreeSet<lash_kernel_doc::FunctionId>,
        functions: &mut Vec<lash_kernel_doc::FunctionId>,
    ) {
        if !seen.insert(function) {
            return;
        }
        if let Some(facts) = analysis.facts(&function)
            && facts.closed
        {
            for callee in facts.callees() {
                if library
                    .definition(callee)
                    .is_some_and(|definition| definition.body().is_some())
                {
                    order(*callee, library, analysis, seen, functions);
                }
            }
        }
        functions.push(function);
    }
    let mut functions = Vec::new();
    let mut seen = BTreeSet::new();
    for function in &library.helpers {
        order(*function, library, &analysis, &mut seen, &mut functions);
    }
    for function in &functions {
        let Some(definition) = library.definition(function) else {
            continue;
        };
        let Some(facts) = analysis.facts(function) else {
            continue;
        };
        // A body that reaches guest code compiles to a resumable segment
        // (FIG-6709); one that reaches a task action or the host stays a
        // body.
        let segment = !facts.closed;
        let outcome = if segment && facts.kind() != "callback" {
            Err(format!("reaches {}", facts.kind()))
        } else {
            let ident = ident(definition.name.as_str(), &mut taken);
            emit::emit(library, &answering, *function, definition, &ident, segment)
                .map(|plan| (ident, plan))
                .map_err(|unsupported| unsupported.0)
        };
        match outcome {
            Ok((ident, plan)) => {
                let generated = if plan.sites == 0 {
                    answering.insert(*function);
                    "generated".to_string()
                } else {
                    format!("segment of {} sites", plan.sites)
                };
                plans.push((ident, plan));
                outcomes.insert(*function, generated);
            }
            Err(reason) => {
                outcomes.insert(*function, reason);
            }
        }
    }

    let mut rust = String::new();
    let _ = writeln!(rust, "// @generated by `{REGENERATE}`; do not edit.");
    let _ = writeln!(
        rust,
        "//! The {} helpers' plans, compiled from their kernel bodies (FIG-6678, FIG-6709).",
        dialect.name()
    );
    rust.push_str(
        "\n#![allow(\n    clippy::collapsible_if,\n    clippy::needless_return,\n    clippy::never_loop,\n    clippy::redundant_closure_call,\n    clippy::unnecessary_wraps,\n    unreachable_code,\n    unused_assignments,\n    unused_labels,\n    unused_mut\n)]\n\n",
    );
    rust.push_str("use lash_kernel_doc::Value;\nuse lash_kernel_doc::plan as p;\n\n");
    rust.push_str(
        "#[allow(dead_code)]\nenum Departure { Next, Return(Value), Break, Continue }\n\n",
    );
    rust.push_str("/// Every plan, in the order the helpers are registered.\n");
    rust.push_str("pub const PLANS: &[&p::HelperPlan] = &[\n");
    for (ident, _) in &plans {
        let _ = writeln!(rust, "    &{},", ident.to_uppercase());
    }
    rust.push_str("];\n");
    for (_, plan) in &plans {
        rust.push('\n');
        rust.push_str(&plan.item);
        rust.push('\n');
        rust.push_str(&plan.code);
    }

    let mut manifest = String::new();
    let _ = writeln!(manifest, "# @generated by `{REGENERATE}`; do not edit.");
    manifest.push_str(
        "name\tfunction\tkind\tcallback_sites\tmutates\tclosures\tmetered\tformula\tplan\n",
    );
    for function in &functions {
        let (Some(definition), Some(facts)) =
            (library.definition(function), analysis.facts(function))
        else {
            continue;
        };
        let sites = facts.callback_sites();
        let _ = writeln!(
            manifest,
            "{}\t{function}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            definition.name,
            facts.kind(),
            if sites.is_empty() {
                "-".to_string()
            } else {
                sites.join("; ")
            },
            facts.mutates,
            facts.closures(),
            facts.metered(),
            analysis::formula(&definition.charge),
            outcomes.get(function).map_or("-", String::as_str),
        );
    }
    Ok(Generated {
        rust,
        manifest,
        planned: plans.len(),
    })
}

/// Where `dialect`'s output is checked in, from the repository root.
pub fn paths(dialect: Dialect) -> (&'static str, &'static str) {
    match dialect {
        Dialect::Python => (
            "crates/lash-ext-py-core/src/generated/plans.rs",
            "crates/lash-ext-py-core/src/generated/plans.tsv",
        ),
        Dialect::TypeScript => (
            "crates/lash-ext-ts-core/src/generated/plans.rs",
            "crates/lash-ext-ts-core/src/generated/plans.tsv",
        ),
    }
}
