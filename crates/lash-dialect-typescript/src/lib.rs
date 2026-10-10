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

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH, ParserStack};
pub use builtins::{Receiver, Row};
pub use diagnostics::{CodeClassification, Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};
pub use package::define_helpers;
pub use printer::print;

use lash_kernel_dialect::{Environment, FrontEnd, Lowered};

/// Lowers TypeScript source to a kernel document.
///
/// The source is a cell: its top-level `let`, `const`, `var` and function
/// bindings are the session's (`K-SES-001`), and `environment.bindings`
/// names those earlier cells left.
pub fn lower(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    lower::check_binding_names(
        environment
            .bindings
            .iter()
            .map(lash_kernel_doc::Name::as_str),
        environment.effects,
    )?;
    let program = adapter::parse(source)?;
    lower::lower(&program, source, environment)
}

/// Reads exact kernel text: the reserved source surface a printer writes
/// ([`print`]), with kernel semantics and no dialect helper. It is a trusted
/// host entry: kernel text can state forms no cell may write, `finish`
/// among them, so nothing a model wrote is read through it. [`lower`] never
/// selects it.
///
/// # Errors
///
/// The diagnostic of source that is not exact kernel text.
pub fn lower_kernel_text(
    source: &str,
    environment: &Environment<'_>,
) -> Result<Lowered, Diagnostic> {
    intrinsics::lower(source, environment).unwrap_or_else(|| {
        Err(Diagnostic::new(
            DiagnosticCode::InvalidAst,
            "the source is not exact kernel text",
            None,
        ))
    })
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
pub struct TypeScript;

impl FrontEnd for TypeScript {
    fn lower(
        &self,
        source: &str,
        environment: &Environment<'_>,
    ) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
        lower(source, environment).map_err(Into::into)
    }
}
