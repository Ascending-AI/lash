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

mod captions;
mod diagnostics;
mod exceptions;
mod generated_operators;
mod lower;
mod package;
mod printer;
mod readable;
mod scope;

#[cfg(test)]
mod tests;

pub use diagnostics::{Code, GeneratedSourceCode};
pub use lash_kernel_dialect::{
    CaptionSupport, GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat,
};
pub use package::define_helpers;
pub use readable::{print, read};

/// The captions the dialect spells: every kind, as `#:` comment lines
/// directly above what they caption. A workflow module's front end, the
/// reader of generated source and the printer share the syntax; a cell
/// has none.
pub const CAPTIONS: CaptionSupport = CaptionSupport {
    kinds: &lash_kernel_doc::CaptionKind::ALL,
    guide: captions::GUIDE,
};

use lash_kernel_dialect::{Diagnostic, Dialect, Environment, FrontEnd, Lowered, Mode};
use lash_kernel_doc::Span;
use ruff_text_size::Ranged;

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
    max_annotation_nodes: usize,
}
impl SourceLimits {
    /// Bound each annotation's expanded structural nodes (default 4096).
    /// Structural depth uses `max_nesting` (default 96).
    pub fn with_annotation_nodes(mut self, max_nodes: usize) -> Self {
        self.max_annotation_nodes = max_nodes;
        self
    }

    pub const fn standard() -> Self {
        Self {
            max_bytes: MAX_SOURCE_BYTES,
            max_nesting: 96,
            max_annotation_nodes: 4096,
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
            max_annotation_nodes: 4096,
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
/// In [`Mode::Cell`] the source is a cell: the names it binds at its top
/// level are the session's (`K-SES-001`), and the cell environment's
/// bindings name those earlier cells left. A cell may `await` at its top
/// level.
///
/// In [`Mode::Module`] it is a whole workflow a host imports: every
/// top-level `async def` is an entry of the document under the shared
/// workflow contract ([`lash_kernel_dialect::workflow`]), and the rest of
/// the top level is `main`. A module runs no effect and lowers to an
/// unchecked document; a host must admit it before use.
pub fn lower(
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
) -> Result<Lowered, Diagnostic> {
    lower_with_limits(source, environment, mode, SourceLimits::standard())
}
/// Lower under deployment source admission limits.
pub fn lower_with_limits(
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    limits: SourceLimits,
) -> Result<Lowered, Diagnostic> {
    lower_with_calls(source, environment, mode, limits, None, &[])
}

fn lower_with_calls(
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    limits: SourceLimits,
    calls: Option<&mut std::collections::BTreeSet<String>>,
    beside: &[String],
) -> Result<Lowered, Diagnostic> {
    if let Mode::Cell(cell) = mode {
        lower::check_binding_names(
            cell.bindings
                .iter()
                .map(lash_vocabulary::names::Name::as_str),
        )?;
    }
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
        ..Default::default()
    })?;
    let spelled = captions::spelled(
        source,
        parsed
            .tokens()
            .iter()
            .filter(|token| token.kind() == ruff_python_ast::token::TokenKind::Comment)
            .map(|token| token.start().to_usize()..token.end().to_usize()),
    );
    lower::lower_with_calls(
        parsed.syntax(),
        source,
        environment,
        mode,
        limits,
        calls,
        spelled,
        beside,
    )
}

/// The Python front end, as a dialect package holds it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Python {
    pub limits: SourceLimits,
}

impl FrontEnd for Python {
    fn tool_calls(
        &self,
        source: &str,
        environment: &Environment<'_>,
        cell: lash_kernel_dialect::CellEnvironment<'_>,
    ) -> Result<lash_kernel_dialect::ToolCalls, Diagnostic> {
        let mut calls = std::collections::BTreeSet::new();
        lower_with_calls(
            source,
            environment,
            Mode::Cell(cell),
            self.limits,
            Some(&mut calls),
            &[],
        )?;
        Ok(lash_kernel_dialect::ToolCalls {
            calls,
            namespace_collisions: environment
                .namespaces
                .collisions(cell.bindings, lash_kernel_dialect::Spelling::Python),
        })
    }

    fn lower(
        &self,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Result<Lowered, Diagnostic> {
        lower_with_limits(source, environment, mode, self.limits)
    }

    fn reserves(&self, root: &str) -> Option<String> {
        if lash_kernel_dialect::value_constructors::Constructor::reserves(root) {
            return Some("a typed value constructor".into());
        }
        if lower::is_builtin(root) || root == "asyncio" {
            Some("a Python built-in".to_owned())
        } else if !printer::legal_identifier(root) {
            Some("a name the Python printer generates".to_owned())
        } else {
            None
        }
    }
}

/// The dialect as an embedder installs it: the front end, with its exact
/// generated-source printer. Its helpers are lash's `python` package.
pub fn dialect() -> Dialect {
    Dialect {
        name: DIALECT.to_string(),
        front_end: Box::new(Python::default()),
        read,
        printer: Some(Box::new(Python::default())),
        captions: CAPTIONS,
        function_values: Some(lash_kernel_doc::FunctionRepresentation::Bare),
    }
}

mod generated_scope;
