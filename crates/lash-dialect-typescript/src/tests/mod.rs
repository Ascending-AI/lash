use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use lash_kernel_dialect::{CellEnvironment, Environment, Lowered, Mode, NamedLibrary};
use lash_kernel_doc::{EffectName, Signature, parse_document, print_document, validate_document};

use crate::Diagnostic;

mod array;
mod array_callbacks;
mod async_fn;
mod builtin_values;
mod controls;
mod deviations;
mod generated_source;
mod guarded_text;
mod hoisting;
mod hot_helpers;
mod json;
mod language;
mod machine;
mod module;
mod naming;
mod operands;
mod package;
mod pages;
mod positional;
mod printer;
mod printer_corpus;
mod process;
mod protocol_loops;
mod readable_source;
mod remaining_builtins;
mod source_map;
mod text_building;
mod trim;
mod typed;

/// Dependencies and the building-artifact function for independent helper definitions.
pub(crate) fn kernel_registry() -> lash_kernel_doc::FunctionRegistry {
    static BASE: OnceLock<lash_kernel_doc::FunctionRegistry> = OnceLock::new();
    BASE.get_or_init(|| {
        lash_ext_helper_compiler::library::Library::of(
            lash_ext_helper_compiler::library::Dialect::TypeScript,
        )
        .expect("source packages")
        .base_registry()
        .expect("package dependencies")
    })
    .clone()
}

/// The exact library definitions the unit machine executes.
pub(crate) fn library() -> &'static NamedLibrary {
    static LIBRARY: OnceLock<NamedLibrary> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        NamedLibrary::from_registry(machine::registry()).expect("unique library names")
    })
}

/// Lowers a first cell and checks what every lowered document must hold:
/// it is admitted, its annotations are its own, and it survives kernel
/// text.
/// The test host's turn-ending tool: `await control.finish(x)` ends the turn with `x`.
/// The laws' cells end through it as a model's cells end through
/// `control.finish`.
pub(crate) fn controls()
-> &'static BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>> {
    static CONTROLS: OnceLock<BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>> =
        OnceLock::new();
    CONTROLS.get_or_init(|| {
        BTreeMap::from([(
            EffectName::new("control.finish").expect("a tool's name"),
            BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
        )])
    })
}

pub(crate) fn lower(source: &str) -> Result<Lowered, Diagnostic> {
    lower_in_session(source, &[])
}

pub(crate) fn lower_in_session(source: &str, bindings: &[&str]) -> Result<Lowered, Diagnostic> {
    lower_against(source, bindings, &BTreeMap::new())
}

/// Lowers a first cell whose host supplies the laws' tools, `echo` and
/// `boom`.
pub(crate) fn lower_with_effects(source: &str) -> Result<Lowered, Diagnostic> {
    lower_against(source, &[], &machine::effects())
}

fn lower_against(
    source: &str,
    bindings: &[&str],
    effects: &BTreeMap<EffectName, Signature>,
) -> Result<Lowered, Diagnostic> {
    let bindings: BTreeSet<_> = bindings.iter().map(|name| (*name).into()).collect();
    let environment_namespaces =
        lash_kernel_dialect::Namespaces::of_signatures(effects, controls())
            .expect("the test's effects are operations");
    let environment = Environment::new(library(), &environment_namespaces);
    let lowered = crate::lower(
        source,
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment {
            bindings: &bindings,
            functions: &std::collections::BTreeMap::new(),
        }),
    )?;
    if let Err(invalid) = validate_document(&lowered.document, library()) {
        panic!("{invalid}\n{}", print_document(&lowered.document));
    }
    let map = lowered
        .source_map
        .as_ref()
        .expect("a lowered cell has its source map");
    if let Err(refused) = map.validate(&lowered.document, source) {
        panic!("{refused}");
    }
    let text = print_document(&lowered.document);
    assert_eq!(
        parse_document(&text).as_ref(),
        Ok(lowered.document.document()),
        "{text}"
    );
    Ok(lowered)
}

/// The body of `main` in kernel text, one statement per line, without the
/// header that lists library functions.
pub(crate) fn main_text(source: &str) -> String {
    main_of(lower(source))
}

/// [`main_text`] of a cell that may call the laws' tools.
pub(crate) fn main_text_with_effects(source: &str) -> String {
    main_of(lower_with_effects(source))
}

fn main_of(lowered: Result<Lowered, Diagnostic>) -> String {
    let lowered = match lowered {
        Ok(lowered) => lowered,
        Err(diagnostic) => panic!("{diagnostic}"),
    };
    let text = print_document(&lowered.document);
    let start = text.find("main {").expect("a document has a main");
    text[start..]
        .lines()
        .skip(1)
        .take_while(|line| *line != "}")
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// SOURCE-ADMISSION-POLICY: source byte limits are deployment choices, independently of nesting safety.
#[test]
fn a_host_source_policy_allows_a_larger_typescript_cell() {
    let source = format!("//{}", "x".repeat(crate::MAX_SOURCE_BYTES));
    let environment_namespaces =
        lash_kernel_dialect::Namespaces::of_signatures(&Default::default(), &Default::default())
            .expect("the test's effects are operations");
    let environment = Environment::new(library(), &environment_namespaces);
    let cell = Mode::Cell(CellEnvironment::first());
    assert!(crate::lower(&source, &environment, cell).is_err());
    let limits = crate::SourceLimits::new(source.len(), crate::MAX_SOURCE_NESTING_DEPTH).unwrap();
    crate::lower_with_limits(&source, &environment, cell, limits).unwrap();
    let module = format!("{source}\nexport async function main() {{ return null; }}");
    assert!(crate::lower(&module, &environment, Mode::Module).is_err());
    let limits = crate::SourceLimits::new(module.len(), crate::MAX_SOURCE_NESTING_DEPTH).unwrap();
    crate::lower_with_limits(&module, &environment, Mode::Module, limits).unwrap();
    crate::Parser::default()
        .with_limits(limits)
        .lower(&source, &environment, cell)
        .unwrap();
    let limits = crate::SourceLimits::new(source.len(), 1).unwrap();
    let diagnostic =
        crate::lower_with_limits("const x = ((1));", &environment, cell, limits).unwrap_err();
    assert!(diagnostic.message.contains("1-level"), "{diagnostic}");
}
