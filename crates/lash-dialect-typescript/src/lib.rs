//! The TypeScript dialect of the lash kernel.
//!
//! [`lower`] turns TypeScript source into a kernel document. SWC is confined
//! to `adapter`, which reads the source into this crate's own tree and
//! refuses what the dialect does not have; `lower` walks that tree. The
//! operations whose JavaScript meaning differs from the kernel's are calls
//! to the helpers [`define_helpers`] defines, written in kernel text under
//! `helpers/`.

mod adapter;
mod builtins;
mod diagnostics;
mod intrinsics;
mod lower;
mod node_label;
mod package;
mod printer;
mod regex;
mod types;

#[cfg(test)]
mod tests;

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH, ParserStack, SourceLimits};
pub use builtins::{Receiver, Row};
pub use diagnostics::{CodeClassification, Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};
pub use intrinsics::compile_generated;
pub use lash_kernel_dialect::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceMap,
    SourceSite,
};
pub use package::define_helpers;
pub use printer::print;

/// Build provenance for standalone dialect measurement tools.
#[doc(hidden)]
pub const BUILD_IDENTITY: (&str, &str) = (
    env!("KERNEL_PERF_COMPILER"),
    env!("KERNEL_PERF_BUILD_PROFILE"),
);

use lash_kernel_dialect::{Environment, FrontEnd, FunctionValues, Lowered};

/// The tag of the token a TypeScript function value is: a tuple of the tag,
/// the built-in's path, its name and length, the closure that calls it and,
/// for one the program made, the record of what it deleted of itself
/// (`lower/functions.rs`).
pub(crate) const FUNCTION_TAG: &str = "ts.function";

/// What a TypeScript function value is, as the dialect's package declares
/// it ([`lash_kernel_dialect::Package::function_values`]).
pub fn function_values() -> FunctionValues {
    FunctionValues::Token {
        tag: FUNCTION_TAG.to_owned(),
    }
}

/// Lowers TypeScript source to a kernel document.
///
/// The source is a cell: its top-level `let`, `const`, `var` and function
/// bindings are the session's (`K-SES-001`), and `environment.bindings`
/// names those earlier cells left.
pub fn lower(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    lower_with_limits(source, environment, SourceLimits::standard())
}

/// Lower source under the host's byte and nesting admission policy.
pub fn lower_with_limits(
    source: &str,
    environment: &Environment<'_>,
    limits: SourceLimits,
) -> Result<Lowered, Diagnostic> {
    lower::check_binding_names(
        environment
            .bindings
            .iter()
            .map(lash_kernel_doc::Name::as_str),
        environment.effects,
    )?;
    let program = adapter::parse_with_limits(source, limits)?;
    lower::lower(&program, source, environment)
}

/// Lowers a host-authored whole workflow, including the exact source the
/// printer writes. Exported async declarations and const-bound async literals
/// (optionally wrapped in `process(...)`) become document entries even when
/// the environment offers no process-start tools. This runs no effect and
/// returns an unchecked document; a host must admit it before use.
///
/// Unlike cell lowering, this trusted source entry accepts the printer's
/// reserved kernel surface, including `finish`.
pub fn lower_workflow(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    lower_workflow_with_limits(source, environment, SourceLimits::standard())
}

/// Whether trusted whole-workflow dispatch must use the generated decoder.
/// This includes incomplete reserved envelopes so malformed generated modules
/// cannot fall through to authored lowering. It grants no model-cell authority.
pub fn is_generated_workflow(source: &str) -> bool {
    intrinsics::source_probe(source).0
}

/// Lower a whole TypeScript workflow under host source admission policy.
pub fn lower_workflow_with_limits(
    source: &str,
    environment: &Environment<'_>,
    limits: SourceLimits,
) -> Result<Lowered, Diagnostic> {
    if is_generated_workflow(source) {
        let document = compile_generated(source, GeneratedSourceLimits::default())?;
        let mut annotations =
            lash_kernel_doc::Annotations::new(document.identity().map_err(|error| {
                Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None)
            })?);
        annotations.dialect = Some("typescript".into());
        return Ok(Lowered {
            document,
            annotations,
        });
    }
    lower::check_binding_names(
        environment
            .bindings
            .iter()
            .map(lash_kernel_doc::Name::as_str),
        environment.effects,
    )?;
    let program = adapter::parse_workflow(source, limits)?;
    lower::workflow(&program, source, environment)
}

/// Reads `source` as the dialect's syntax and lowers nothing: what a host
/// asks of text it only shows, such as an example in a prompt.
///
/// # Errors
///
/// The diagnostic of source outside the dialect's syntax.
pub fn parse(source: &str) -> Result<(), Diagnostic> {
    adapter::parse(source).map(|_| ())
}

/// A parser one serial caller owns and reuses.
///
/// The first source starts one thread with the stack a maximum-sized source
/// needs; later sources reuse it. No source and no tree is kept between
/// calls, and limits and diagnostics are those of [`lower`].
#[derive(Default)]
pub struct Parser {
    parser: adapter::Parser,
}

impl Parser {
    /// Chooses how the parser thread's stack is reserved, before it starts.
    pub fn with_stack(stack: ParserStack) -> Self {
        Self {
            parser: adapter::Parser::with_stack(stack),
        }
    }

    /// Set admission before starting the reusable parser thread.
    pub fn with_limits(self, limits: SourceLimits) -> Self {
        Self {
            parser: self.parser.with_limits(limits),
        }
    }

    pub fn lower(
        &mut self,
        source: &str,
        environment: &Environment<'_>,
    ) -> Result<Lowered, Diagnostic> {
        lower::check_binding_names(
            environment
                .bindings
                .iter()
                .map(lash_kernel_doc::Name::as_str),
            environment.effects,
        )?;
        let program = self.parser.parse(source)?;
        lower::lower(&program, source, environment)
    }
}

/// Lowers `source` as a first cell against the dialect's own helpers.
#[cfg(test)]
pub(crate) fn validate(source: &str) -> Result<(), Diagnostic> {
    tests::lower(source).map(|_| ())
}

/// The TypeScript front end, as a dialect package holds it.
#[derive(Clone, Copy, Debug, Default)]
pub struct TypeScript {
    pub limits: SourceLimits,
}

impl FrontEnd for TypeScript {
    fn lower(
        &self,
        source: &str,
        environment: &Environment<'_>,
    ) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
        lower_with_limits(source, environment, self.limits).map_err(Into::into)
    }
}
