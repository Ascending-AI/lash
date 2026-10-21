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
mod captions;
mod diagnostics;
mod lower;
mod package;
mod printer;
mod readable;
mod regex;
mod types;

#[cfg(test)]
mod tests;

pub use adapter::{MAX_SOURCE_BYTES, MAX_SOURCE_NESTING_DEPTH, ParserStack, SourceLimits};
pub use builtins::{Receiver, Row};
pub use diagnostics::{CodeClassification, Diagnostic, DiagnosticCode, DiagnosticKind};
pub use lash_kernel_dialect::{
    CaptionSupport, GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat,
};
pub use package::{builtin_names, define_helpers};
pub use readable::{print, read};

/// The captions the dialect spells: every kind, as a JSDoc block before the
/// statement, function, `else`/`catch`/`finally` keyword, parameter or
/// property of an inline object type it names. A workflow module's front
/// end, the reader of exported source and the printer spell them alike; a
/// cell's front end reads none.
pub const CAPTIONS: CaptionSupport = CaptionSupport {
    kinds: captions::KINDS,
    guide: captions::GUIDE,
};

/// Build provenance for standalone dialect measurement tools.
#[doc(hidden)]
pub const BUILD_IDENTITY: (&str, &str) = (
    env!("KERNEL_PERF_COMPILER"),
    env!("KERNEL_PERF_BUILD_PROFILE"),
);

use lash_kernel_dialect::{Environment, FrontEnd, Lowered, Mode};
use lash_kernel_doc::Span as SourceSpan;

/// The tag of the token a TypeScript function value is: a tuple of the tag,
/// the built-in's path, its name and length, the closure that calls it and,
/// for one the program made, the record of what it deleted of itself
/// (`lower/functions.rs`).
pub(crate) const FUNCTION_TAG: &str = "ts.function";

/// The slot of a TypeScript function token that holds its closure.
pub(crate) const FUNCTION_CALLABLE: u32 = 4;

/// What a TypeScript function value is, as the dialect's package declares
/// it ([`lash_kernel_dialect::Dialect::function_values`]).
pub fn function_values() -> lash_kernel_doc::FunctionRepresentation {
    lash_kernel_doc::FunctionRepresentation::Token {
        tag: FUNCTION_TAG.to_owned(),
        callable: FUNCTION_CALLABLE,
    }
}

/// Lowers TypeScript source to a kernel document.
///
/// In [`Mode::Cell`] the source is a cell: its top-level `let`, `const`,
/// `var` and function bindings are the session's (`K-SES-001`), and the
/// cell environment's bindings name those earlier cells left.
///
/// In [`Mode::Module`] it is a whole workflow a host imports: every
/// top-level async function, by declaration (exported or not) or bound by
/// `const`, is an entry of the document under the shared workflow contract
/// ([`lash_kernel_dialect::workflow`]), and the rest of the top level is
/// `main`. Exported source printed from a document is read back with
/// [`read`] against that document instead. A module runs no effect and
/// lowers to an unchecked document; a host must admit it before use.
/// Printed source follows the same ordinary authored grammar.
pub fn lower(
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
) -> Result<Lowered, Diagnostic> {
    lower_with_limits(source, environment, mode, SourceLimits::standard())
}

/// Lower source under the host's byte and nesting admission policy.
pub fn lower_with_limits(
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    limits: SourceLimits,
) -> Result<Lowered, Diagnostic> {
    match mode {
        Mode::Cell(cell) => {
            lower::check_binding_names(cell.bindings.iter().map(lash_vocabulary::Name::as_str))?;
            let program = adapter::parse_with_limits(source, limits)?;
            lower::lower(&program, source, environment, mode)
        }
        Mode::Module => {
            let program = adapter::parse_workflow(source, limits)?;
            lower::lower(&program, source, environment, mode)
        }
    }
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

    /// Finds free dotted tool calls in lexical scopes without lowering.
    /// A lowering refusal elsewhere in the parsed cell cannot hide a call.
    pub fn tool_calls(
        &mut self,
        source: &str,
        environment: &Environment<'_>,
        cell: lash_kernel_dialect::CellEnvironment<'_>,
    ) -> Result<std::collections::BTreeSet<String>, Diagnostic> {
        lower::check_binding_names(cell.bindings.iter().map(lash_vocabulary::Name::as_str))?;
        let program = self.parser.parse(source)?;
        lower::tool_calls(&program, cell.bindings)
            .into_iter()
            .filter_map(|path| candidate(&path, environment).transpose())
            .collect()
    }

    /// Lowers as [`lower`] does. A cell reuses the parser thread; a module,
    /// which a host imports once, is read on a thread of its own.
    pub fn lower(
        &mut self,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Result<Lowered, Diagnostic> {
        let Mode::Cell(cell) = mode else {
            return lower_with_limits(source, environment, mode, self.parser.limits());
        };
        lower::check_binding_names(cell.bindings.iter().map(lash_vocabulary::Name::as_str))?;
        let program = self.parser.parse(source)?;
        lower::lower(&program, source, environment, mode)
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
    fn tool_calls(
        &self,
        source: &str,
        environment: &Environment<'_>,
        cell: lash_kernel_dialect::CellEnvironment<'_>,
    ) -> Result<std::collections::BTreeSet<String>, lash_kernel_dialect::Diagnostic> {
        Parser::default()
            .with_limits(self.limits)
            .tool_calls(source, environment, cell)
            .map_err(Into::into)
    }

    fn lower(
        &self,
        source: &str,
        environment: &Environment<'_>,
        mode: Mode<'_>,
    ) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
        lower_with_limits(source, environment, mode, self.limits).map_err(Into::into)
    }

    fn reserves(&self, root: &str) -> Option<String> {
        if builtins::is_global(root) {
            Some("a TypeScript global".to_owned())
        } else if !printer::legal_identifier(root) {
            Some("a name the TypeScript printer generates".to_owned())
        } else {
            None
        }
    }
}

/// The canonical operation a free dotted call `path` would name, as a
/// candidate for the run's deferred provider: each segment unspelled, so
/// `inbox.sendItem` is `inbox.send_item`. `None` for a path that names no
/// operation. A path that spells an operation in another dialect's spelling
/// is refused with the spelling a cell writes.
fn candidate(path: &str, environment: &Environment<'_>) -> Result<Option<String>, Diagnostic> {
    use lash_kernel_dialect::{SpellKind, Spelling};
    use lash_vocabulary::Segment;
    let segments = path.split('.').collect::<Vec<_>>();
    let Some((name, package)) = segments.split_last() else {
        return Ok(None);
    };
    if package.is_empty() || !package.iter().all(|segment| Segment::is_canonical(segment)) {
        return Ok(None);
    }
    if let Some(misspelled) = environment
        .namespaces
        .misspelled(&segments, Spelling::TypeScript)
    {
        return Err(Diagnostic::with_repair(
            DiagnosticCode::OperationSpelling,
            format!(
                "`{path}` names the operation `{}` in another dialect's spelling",
                misspelled.operation.effect_name()
            ),
            format!("write `{}`", misspelled.repair),
            None,
        ));
    }
    if let Some(canonical) = Spelling::TypeScript.unspell(SpellKind::Member, name) {
        return Ok(Some(format!("{}.{canonical}", package.join("."))));
    }
    match Segment::new(*name) {
        Ok(canonical) => Err(Diagnostic::with_repair(
            DiagnosticCode::OperationSpelling,
            format!("`{path}` spells an operation name in another dialect's spelling"),
            format!(
                "write `{}.{}`",
                package.join("."),
                Spelling::TypeScript.spell(SpellKind::Member, &canonical)
            ),
            None,
        )),
        Err(_) => Ok(None),
    }
}
