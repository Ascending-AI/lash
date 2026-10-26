//! The helper compiler (FIG-6678, FIG-6709): each dialect's helpers,
//! compiled from their kernel bodies to Rust plans the machine runs where a
//! native would answer ([`lash_kernel_doc::plan`]).
//!
//! The bodies stay the definitions. The compiler reads them as a worker
//! registers them, finds which are closed and which reach guest code or
//! perform ([`analysis`]), and emits a plan for each body whose every form
//! it performs as the body does ([`emit`]): a closed body runs to its end,
//! and one that reaches guest code or performs is a resumable segment that
//! yields at each callback site, a `perform` among them, and resumes after
//! it. Every other body runs as it is. The
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
mod unicode;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use lash_kernel_dialect::Library as _;

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
    ident = ident
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
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

/// The Rust constant holding the plan whose code is named `ident`. The fixed
/// `_PLAN` suffix keeps a helper such as Python's `format` from reading as a
/// stored-format version constant (FIG-6884).
fn plan_const(ident: &str) -> String {
    format!("{}_PLAN", ident.to_uppercase())
}

/// Compiles `dialect`'s helpers.
///
/// # Errors
///
/// A refusal of the helpers' registration, as text.
pub fn generate(dialect: Dialect) -> Result<Generated, String> {
    let library = Library::of(dialect)?;
    let mut generated = generate_library(&library, dialect)?;
    if dialect == Dialect::Python {
        generated.rust.push_str(&unicode::tables(&library)?);
    }
    if dialect == Dialect::TypeScript {
        // The fixed table `ts.builtin_name`'s native searches, as the
        // helper's body does.
        generated.rust.push_str(
            "\n/// Every name a computed read finds among the built-in rows, in code\n\
             /// point order: what `ts.builtin_name` searches.\n\
             pub const BUILTIN_NAMES: &[&str] = &[\n",
        );
        for name in lash_ext_ts_helpers::package::builtin_names() {
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
        function: lash_kernel_value::names::FunctionId,
        library: &Library,
        analysis: &Analysis,
        seen: &mut BTreeSet<lash_kernel_value::names::FunctionId>,
        functions: &mut Vec<lash_kernel_value::names::FunctionId>,
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
        // Lifted closure bodies stay in the catalog for their factories,
        // but are not independently callable helper-package members.
        if !library.definition(function).is_some_and(|definition| {
            library.names.resolve(definition.name.as_str()) == Some(*function)
        }) {
            continue;
        }
        order(*function, library, &analysis, &mut seen, &mut functions);
    }
    for function in &functions {
        let Some(definition) = library.definition(function) else {
            continue;
        };
        let Some(facts) = analysis.facts(function) else {
            continue;
        };
        // A body that reaches guest code or performs compiles to a
        // resumable segment (FIG-6709, FIG-6916); one that reaches only a
        // task action or a host read stays a body.
        let segment = !facts.closed;
        let outcome = if segment && !facts.segment() {
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
                plans.push((*function, ident, plan));
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
    rust.push_str("/// Every plan, in function identity order.\n");
    rust.push_str("pub const PLANS: &[&p::HelperPlan] = &[\n");
    // Registration's identity order is pure build metadata. Keep code
    // emission in dependency order; only the lookup index needs sorting.
    let mut index: Vec<_> = plans.iter().collect();
    index.sort_unstable_by_key(|(function, _, _)| *function);
    for (_, ident, _) in index {
        let _ = writeln!(rust, "    &{},", plan_const(ident));
    }
    rust.push_str("];\n");
    for (_, _, plan) in &plans {
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
            definition
                .charge()
                .map(analysis::formula)
                .unwrap_or_else(|| "statements".to_owned()),
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

/// Where the body-perform laws' plans are checked in, from the repository
/// root (FIG-6916).
pub const BODY_PERFORM_PLANS: &str =
    "crates/lash-kernel-vm/src/laws/continuation/body_performs_plans.rs";

/// The plans of the kernel body-perform laws' library bodies, `source`
/// (FIG-6916): the laws run each body and its plan, compiled by this
/// compiler as it is now, and compare them.
///
/// # Errors
///
/// A refusal of the bodies' registration or compilation, as text.
pub fn body_perform_plans(source: &str) -> Result<String, String> {
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    // The laws' library's `list.len`, whose native the laws register.
    let len = lash_kernel_doc::parse_definition(
        "function list.len(xs: Any) -> Int\nkernel 1\ncharge 1\nnative\n",
    )
    .map_err(|error| error.to_string())?;
    registry
        .register(len, None)
        .map_err(|error| error.to_string())?;
    let mut names = lash_kernel_dialect::NamedLibrary::from_registry(&registry)
        .map_err(|error| error.to_string())?;
    let definitions = lash_kernel_dialect::define_functions(source, &mut names, &[])
        .map_err(|error| error.to_string())?;
    let mut helpers = Vec::with_capacity(definitions.len());
    for definition in definitions {
        helpers.push(
            registry
                .register(definition, None)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(generate_library(
        &Library {
            names,
            registry,
            helpers,
        },
        Dialect::Python,
    )?
    .rust)
}

/// Registers lash's kernel library, natives included: what the `dataframe`
/// fixture's kernel package holds here and in the kernel laws that run its
/// plan.
///
/// # Errors
///
/// A refusal of the registration, as text.
pub fn register_kernel(registry: &mut lash_kernel_doc::FunctionRegistry) -> Result<(), String> {
    lash_kernel_vm::register_machine_functions(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_numbers(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_text_json(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_collections(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_brands(registry).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::{REGENERATE, body_perform_plans};

    /// The body-perform laws run the plans this compiler gives now.
    #[test]
    fn body_perform_law_plans_are_what_the_compiler_gives_for_the_current_bodies() {
        assert_eq!(
            body_perform_plans(include_str!(
                "../../lash-kernel-vm/src/laws/continuation/body_performs.kernel"
            ))
            .expect("the body-perform law bodies compile"),
            include_str!("../../lash-kernel-vm/src/laws/continuation/body_performs_plans.rs"),
            "run {REGENERATE}"
        );
    }
}

#[cfg(test)]
mod dialect_laws;
