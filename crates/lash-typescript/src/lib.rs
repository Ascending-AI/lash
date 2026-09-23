//! TypeScript dialect front-end for the Lash heap VM.
//!
//! SWC is an implementation detail confined to `adapter`; callers receive the
//! shared Lash AST, compiled program, or a stable named diagnostic.

mod adapter;
mod diagnostics;
mod lower;
mod node_label;
mod regex;
mod signatures;
pub mod workflow_graph;

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH};

/// Exists so a test can demonstrate that the no-abort guarantee does not depend on the
/// preflight.
#[cfg(feature = "testing")]
pub fn parse_without_nesting_preflight(source: &str) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse_without_nesting_preflight(source)?;
    lower::lower(&normalized)
}

/// Parses on the caller's own stack with no guard at all, for measuring how
/// much stack the parser needs per source byte.
#[cfg(feature = "testing")]
pub fn parse_unguarded_for_measurement(source: &str) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse_unguarded(source)?;
    lower::lower(&normalized)
}
/// The source language this front end records on every program it lowers.
pub const TYPESCRIPT_LANGUAGE: &str = "typescript";

/// The prefix on every binding the lowerer generates. Source identifiers that
/// start with it are rejected. It is this front end's own namespace: the
/// lowered program marks every generated binding private, so no caller needs
/// to recognise the prefix.
pub(crate) use lower::GENERATED_BINDING_PREFIX;

/// Exposed so the register's documented inventory can be pinned against the
/// allowlist instead of being maintained by hand.
pub fn accepts_instance_method(method: &str) -> bool {
    lower::accepts_instance_method(method)
}

/// Every instance standard-library method the lowerer accepts.
///
/// Exposed alongside the predicate so the register's pin can assert set
/// equality. A predicate alone only answers questions that are asked, which
/// leaves the direction that actually drifted — the allowlist growing while
/// the register stands still — checked by spot samples.
pub fn accepted_instance_methods() -> &'static [&'static str] {
    lower::accepted_instance_methods()
}

pub use diagnostics::{
    CodeClassification, Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan, format_diagnostic,
};
pub use signatures::{
    ensure_tool_call_path_addressable, render_schema_type, render_stdlib_contract, reserved_words,
    stdlib_name_count,
};

pub fn parse(source: &str) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse(source)?;
    lower::lower(&normalized)
}

/// Parses a cell that runs in a session which already has `globals` bound.
///
/// A standalone program is self-contained, which is what [`parse`] compiles. A
/// *cell* is not: the session model persists top-level bindings across cells,
/// and the prompt shows them to the model with their values. Passing the live
/// names here is what lets cell B read what cell A bound; a name in neither the
/// source nor `globals` still rejects as `TS_UNKNOWN_BINDING`.
pub fn parse_with_globals(
    source: &str,
    globals: &std::collections::BTreeSet<String>,
) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse(source)?;
    lower::lower_with_ambient(&normalized, globals, &std::collections::BTreeSet::new())
}

/// The second set is semantic binding metadata: it keeps an ambient handle awaitable without
/// making arbitrary ambient values awaitable.
pub fn parse_with_globals_and_process_handles(
    source: &str,
    globals: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse(source)?;
    lower::lower_with_ambient(&normalized, globals, process_handles)
}

/// Parses one editable workflow-graph fragment with `globals` already bound.
///
/// Unlike a cell, a fragment was cut out of a program the lens projected: its
/// ambient names are the bindings live where the fragment sits, and a fragment
/// that reassigns one of them is ordinary edited source, not a const violation.
/// `processes` names the process bodies the module declares, which a fragment
/// may start through the catalogue tools.
pub fn parse_workflow_fragment(
    source: &str,
    globals: &std::collections::BTreeSet<String>,
    processes: &std::collections::BTreeSet<String>,
) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse(source)?;
    lower::lower_workflow_fragment(&normalized, globals, processes)
}

pub fn validate(source: &str) -> Result<(), Diagnostic> {
    parse(source).map(|_| ())
}

pub fn link(
    source: &str,
    host: &lashlang::LashlangHostEnvironment,
) -> Result<lashlang::LinkedModule, Diagnostic> {
    // The host environment already carries the session globals and module
    // catalog, so lowering reads them from the same surface the linker will.
    let normalized = adapter::parse(source)?;
    let module_authority_roots = host
        .resources
        .module_instances()
        .filter_map(|(_, module)| module.path.first().cloned())
        .collect();
    let program = lower::lower_with_context(
        &normalized,
        &host.globals,
        &host.process_handles,
        &module_authority_roots,
    )?;
    lashlang::LinkedModule::link(program, host)
        .map_err(|error| Diagnostic::new(DiagnosticCode::LinkError, error.to_string(), None))
}

/// Test support: compiles TypeScript the way a test that wants bytecode for a
/// standalone program needs it, through the one artifact compile entry.
#[cfg(feature = "testing")]
pub mod testing {
    use crate::{Diagnostic, DiagnosticCode};

    /// Parses `source` and compiles it as the main entry of the raw module
    /// artifact it forms. Source spans are kept for runtime diagnostics.
    pub fn compile(source: &str) -> Result<lashlang::CompiledProgram, Diagnostic> {
        let program = crate::parse(source)?;
        let spans = program.spans.clone();
        let artifact = lashlang::ModuleArtifact::from_program(program).map_err(|error| {
            Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None)
        })?;
        lashlang::compile(&artifact, lashlang::Entry::Main, Some(&spans))
            .map_err(|error| Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None))
    }
}
