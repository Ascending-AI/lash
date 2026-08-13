//! TypeScript dialect front-end for the Lash heap VM.
//!
//! SWC is an implementation detail confined to `adapter`; callers receive the
//! shared Lash AST, compiled program, or a stable named diagnostic.

mod adapter;
mod diagnostics;
mod lower;
mod signatures;

pub use adapter::MAX_SOURCE_NESTING_DEPTH;
pub use diagnostics::{Diagnostic, DiagnosticCode, SourceSpan};
pub use signatures::render_tool_signature;

/// Parses and lowers a TypeScript dialect program into the VM's shared AST.
pub fn parse(source: &str) -> Result<lashlang::Program, Diagnostic> {
    let normalized = adapter::parse(source)?;
    lower::lower(&normalized)
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
    let program = parse(source)?;
    lashlang::LinkedModule::link(program, host)
        .map_err(|error| Diagnostic::new(DiagnosticCode::LinkError, error.to_string(), None))
}

/// Compiles an already-linked TypeScript module with reference semantics.
pub fn compile_linked(linked: &lashlang::LinkedModule) -> lashlang::CompiledProgram {
    lashlang::compile_linked_with_dialect(linked, lashlang::CompilationDialect::Typescript)
}
