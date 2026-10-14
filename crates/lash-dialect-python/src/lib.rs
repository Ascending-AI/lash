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
mod generated;
mod generated_operators;
mod lower;
mod package;
mod printer;
mod scope;

#[cfg(test)]
mod tests;

pub use diagnostics::{Code, GeneratedSourceCode};
pub use generated::compile_generated;
pub use lash_kernel_dialect::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceMap,
    SourceSite,
};
pub use package::define_helpers;
pub use printer::print;

use lash_kernel_dialect::{Diagnostic, Environment, FrontEnd, Lowered, Package, Span};

/// The name annotations record a document's dialect under.
pub const DIALECT: &str = "python";

/// The largest source the front end reads.
pub const MAX_SOURCE_BYTES: usize = 1 << 20;

/// Deployment source admission. Nesting may be reduced below the native
/// lowerer's 96-level hard ceiling; source bytes are host allocation policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLimits {
    pub max_bytes: usize,
    max_nesting: usize,
}
impl SourceLimits {
    pub const fn standard() -> Self {
        Self {
            max_bytes: MAX_SOURCE_BYTES,
            max_nesting: 96,
        }
    }
    pub fn new(max_bytes: usize, max_nesting: usize) -> Result<Self, Diagnostic> {
        if max_nesting == 0 || max_nesting > 96 {
            return Err(diagnostics::unplaced(
                Code::TooDeep,
                "Python source nesting policy requires 1..=96 levels",
            ));
        }
        Ok(Self {
            max_bytes,
            max_nesting,
        })
    }
}
impl Default for SourceLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Lowers Python source to a kernel document.
///
/// The source is a cell: the names it binds at its top level are the
/// session's (`K-SES-001`), and `environment.bindings` names those earlier
/// cells left. A cell may `await` at its top level.
pub fn lower(source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    lower_with_limits(source, environment, SourceLimits::standard())
}
/// Lower under deployment source admission limits.
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
        environment.tool_roots,
    )?;
    let max_bytes = limits.max_bytes;
    if source.len() > max_bytes {
        return Err(diagnostics::unplaced(
            Code::SourceTooLarge,
            format!("the source is larger than the configured {max_bytes}-byte source limit"),
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
    lower::lower(parsed.syntax(), source, environment, limits.max_nesting)
}

/// The Python front end, as a dialect package holds it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Python {
    pub limits: SourceLimits,
}

impl FrontEnd for Python {
    fn lower(&self, source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
        lower_with_limits(source, environment, self.limits)
    }
}

/// The dialect as an embedder installs it: the front end and the helpers
/// in `functions`, in registration order, with its exact generated-source printer.
pub fn package(functions: Vec<lash_kernel_doc::FunctionDefinition>) -> Package {
    Package {
        dialect: DIALECT.to_string(),
        front_end: Box::new(Python::default()),
        compile_generated,
        printer: Some(Box::new(Python::default())),
        function_values: Some(lash_kernel_dialect::FunctionValues::Bare),
        functions,
    }
}

mod generated_scope;
