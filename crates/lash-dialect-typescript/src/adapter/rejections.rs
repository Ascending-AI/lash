//! Turning an SWC parse position or an unsupported node into a diagnostic.
//!
//! The adapter walks the SWC tree and refuses what the dialect does not have;
//! these are the four ways it says so. They live together because they share
//! one decision — [`DiagnosticCode::classification`] — and because two of them
//! exist only to make that decision explicit at sites whose code cannot make it
//! alone.

use swc_common::Spanned;

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// A parse failure, classified by the parser's structured error kind.
pub(super) fn parser_diagnostic(error: swc_ecma_parser::error::Error) -> Diagnostic {
    use swc_ecma_parser::error::SyntaxError;

    let code = match error.kind() {
        SyntaxError::WithInStrict | SyntaxError::TS2410 => DiagnosticCode::WithUnsupported,
        _ => DiagnosticCode::SyntaxError,
    };
    Diagnostic::new(code, error.kind().msg(), Some(source_span(error.span())))
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

#[cfg(test)]
mod tests {
    use super::*;
    use swc_ecma_parser::error::{Error, SyntaxError};

    /// FIG-7254: both typed with errors keep their code; token prose cannot
    /// turn an unrelated syntax error into a with-statement refusal.
    #[test]
    fn parser_codes_follow_the_error_kind() {
        for (kind, code) in [
            (SyntaxError::WithInStrict, DiagnosticCode::WithUnsupported),
            (SyntaxError::TS2410, DiagnosticCode::WithUnsupported),
            (
                SyntaxError::Unexpected {
                    got: "'with' statement".into(),
                    expected: "identifier",
                },
                DiagnosticCode::SyntaxError,
            ),
        ] {
            let span = swc_common::Span::new(swc_common::BytePos(2), swc_common::BytePos(6));
            let error = parser_diagnostic(Error::new(span, kind));
            assert_eq!(error.code(), code, "{error}");
            assert_eq!(error.span(), Some(SourceSpan { start: 2, end: 6 }));
        }
    }
}
