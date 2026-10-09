//! The Python dialect of the lash kernel.
//!
//! [`lower`] turns a subset of Python into a kernel document. The parser is
//! `ruff_python_parser`; nothing of its tree leaves this crate. Where
//! Python's meaning differs from the kernel's, the lowerer emits a call to
//! one of the helpers [`define_helpers`] defines, written in kernel text
//! under `helpers/`. What the dialect does not have it refuses with a
//! [`Code`]. `README.md` states the subset and `deviations.md` where the
//! dialect is deliberately not Python.
//!
//! The dialect needs nothing of the kernel that the kernel did not already
//! have: no form, no value kind and no library function was added for it.

mod diagnostics;
mod exceptions;
mod lower;
mod package;
mod scope;

#[cfg(test)]
mod tests;

pub use diagnostics::Code;
pub use package::define_helpers;

use lash_kernel_dialect::{Diagnostic, Environment, FrontEnd, Lowered, Package, Span};

/// The name annotations record a document's dialect under.
pub const DIALECT: &str = "python";

/// The largest source the front end reads.
pub const MAX_SOURCE_BYTES: usize = 1 << 20;

/// Lowers Python source to a kernel document.
///
/// The source is a cell: the names it binds at its top level are the
/// session's (`K-SES-001`), and `environment.bindings` names those earlier
/// cells left. A cell may `await` at its top level.
pub fn lower(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(diagnostics::unplaced(
            Code::SourceTooLarge,
            format!("the source is larger than {MAX_SOURCE_BYTES} bytes"),
        ));
    }
    let parsed = ruff_python_parser::parse_module(source).map_err(|error| Diagnostic {
        code: Code::Syntax.as_str().to_string(),
        message: error.error.to_string(),
        span: Some(Span {
            start: error.location.start().to_usize(),
            end: error.location.end().to_usize(),
        }),
        kind: Code::Syntax.kind(),
        repairs: Vec::new(),
    })?;
    lower::lower(parsed.syntax(), source, environment)
}

/// The Python front end, as a dialect package holds it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Python;

impl FrontEnd for Python {
    fn lower(&self, source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
        lower(source, environment)
    }
}

/// The dialect as an embedder installs it: the front end and the helpers
/// in `functions`, in registration order. It ships no printer yet.
pub fn package(functions: Vec<lash_kernel_doc::FunctionDefinition>) -> Package {
    Package {
        dialect: DIALECT.to_string(),
        front_end: Box::new(Python),
        printer: None,
        functions,
    }
}
