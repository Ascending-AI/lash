//! Turning an SWC parse position or an unsupported node into a diagnostic.
//!
//! The adapter walks the SWC tree and refuses what the dialect does not have;
//! these are the four ways it says so. They live together because they share
//! one decision — [`DiagnosticCode::classification`] — and because two of them
//! exist only to make that decision explicit at sites whose code cannot make it
//! alone.

use swc_common::Spanned;

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// A parse failure, mapped to a named code.
///
/// `with` is singled out because SWC reports it as a syntax error while the
/// dialect has a code for it; and a source using Python's `raise` gets told
/// what JavaScript spells instead, since that is the mistake a model most often
/// makes here.
pub(super) fn parser_diagnostic(error: swc_ecma_parser::error::Error, source: &str) -> Diagnostic {
    let mut message = error.kind().msg().to_string();
    if source.split_whitespace().any(|word| word == "raise") {
        message.push_str("; JavaScript uses `throw new Error(...)`, not `raise Error(...)`");
    }
    let code = if message.contains("'with' statement") {
        DiagnosticCode::WithUnsupported
    } else {
        DiagnosticCode::SyntaxError
    };
    Diagnostic::new(code, message, Some(source_span(error.span())))
}

/// A construct the dialect does not have, for a code that already says so.
pub(super) fn reject(
    code: DiagnosticCode,
    construct: &str,
    span: Option<SourceSpan>,
) -> Diagnostic {
    Diagnostic::new(code, refusal_message(construct), span)
}

/// [`reject`] for a per-site code that is refusing a construct.
pub(super) fn reject_refusal(
    code: DiagnosticCode,
    construct: &str,
    span: Option<SourceSpan>,
) -> Diagnostic {
    Diagnostic::refusal(code, refusal_message(construct), span)
}

/// [`reject`] for a per-site code reporting malformed code rather than a
/// forbidden construct — SWC's `Invalid` nodes, mostly, which mean the source
/// did not parse into anything the dialect could have accepted or refused.
pub(super) fn reject_defect(
    code: DiagnosticCode,
    construct: &str,
    span: Option<SourceSpan>,
) -> Diagnostic {
    Diagnostic::defect(code, refusal_message(construct), span)
}

fn refusal_message(construct: &str) -> String {
    format!("{construct} are not in the TypeScript dialect")
}

/// SWC byte positions are already offsets into the submitted source, so a span
/// needs no remapping to be reported in the model's own line numbers.
pub(super) fn source_span(span: swc_common::Span) -> SourceSpan {
    SourceSpan {
        start: span.lo.0 as usize,
        end: span.hi.0 as usize,
    }
}
