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
mod lower;
mod node_label;
mod package;
pub mod provisional;
mod types;

#[cfg(test)]
mod tests;

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH, ParserStack};
pub use builtins::{Receiver, Row};
pub use diagnostics::{CodeClassification, Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};
pub use package::define_helpers;

use lash_kernel_dialect::{Environment, FrontEnd, Lowered};

/// Lowers TypeScript source to a kernel document.
///
/// The source is a cell: its top-level `let`, `const`, `var` and function
/// bindings are the session's (`K-SES-001`), and `environment.bindings`
/// names those earlier cells left.
pub fn lower(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    let program = adapter::parse(source)?;
    lower::lower(&program, source, environment)
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
