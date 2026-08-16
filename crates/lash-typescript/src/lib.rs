//! TypeScript dialect front-end for the Lash heap VM.
//!
//! SWC is an implementation detail confined to `adapter`; callers receive the
//! shared Lash AST, compiled program, or a stable named diagnostic.

mod adapter;
mod diagnostics;
mod lower;
mod regex;
mod signatures;

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH};

/// Parses with the source-nesting preflight disabled, leaving the parse-thread
/// stack reservation as the only thing standing between a deep source and the
/// native stack. Exists so a test can demonstrate that the no-abort guarantee
/// does not depend on the preflight.
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
/// The prefix on every binding the lowerer generates. Source identifiers that
/// start with it are rejected, so a name carrying it is always generated — and
/// never something a caller should render back to a user.
pub const GENERATED_BINDING_PREFIX: &str = lower::GENERATED_BINDING_PREFIX;

/// Whether the lowerer accepts `method` as an instance standard-library method.
///
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
pub use signatures::{render_stdlib_contract, render_tool_signature, stdlib_name_count};

/// Parses and lowers a TypeScript dialect program into the VM's shared AST.
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
    lower::lower_with_ambient(&normalized, globals)
}

/// Validates that a source program belongs to the accepted TypeScript dialect.
pub fn validate(source: &str) -> Result<(), Diagnostic> {
    parse(source).map(|_| ())
}

/// Parses, lowers, validates, and compiles a standalone TypeScript program.
pub fn compile(source: &str) -> Result<lashlang::CompiledProgram, Diagnostic> {
    let program = parse(source)?;
    lashlang::compile_ast_with_dialect(&program, lashlang::CompilationDialect::Typescript)
        .map_err(|error| Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None))
}

/// Parses and links TypeScript against a Lash host environment.
pub fn link(
    source: &str,
    host: &lashlang::LashlangHostEnvironment,
) -> Result<lashlang::LinkedModule, Diagnostic> {
    // The host environment already carries the session's live globals, so
    // linking a cell reads them from the same place the linker will.
    let program = parse_with_globals(source, &host.globals)?;
    lashlang::LinkedModule::link_with_dialect(
        program,
        host,
        lashlang::CompilationDialect::Typescript,
    )
    .map_err(|error| Diagnostic::new(DiagnosticCode::LinkError, error.to_string(), None))
}

/// Compiles an already-linked TypeScript module with reference semantics.
pub fn compile_linked(linked: &lashlang::LinkedModule) -> lashlang::CompiledProgram {
    lashlang::compile_linked_with_dialect(linked, lashlang::CompilationDialect::Typescript)
}

/// Compiles one process from a lowered TypeScript module with reference semantics.
pub fn compile_process(
    program: &lashlang::Program,
    process_name: &str,
) -> Result<lashlang::CompiledProgram, lashlang::RuntimeError> {
    lashlang::compile_process_with_dialect(
        program,
        process_name,
        lashlang::CompilationDialect::Typescript,
    )
}
