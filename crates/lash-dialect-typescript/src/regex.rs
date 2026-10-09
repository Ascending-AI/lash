//! Literal validation uses the same extension that executes the pattern.

use lash_ext_regex_ecma::{Engine, SyntaxError};

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

pub(crate) fn validate_literal(
    pattern: &str,
    flags: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    Engine::new(0)
        .check(pattern, flags)
        .map(|_| ())
        .map_err(|error| {
            let code = match &error {
                SyntaxError::PatternTooLong { .. } => DiagnosticCode::RegexPatternTooLong,
                SyntaxError::PatternTooDeep => DiagnosticCode::RegexNestingLimit,
                SyntaxError::UnsupportedFlag { flag: 'd' } => {
                    DiagnosticCode::RegexIndicesFlagUnsupported
                }
                SyntaxError::UnsupportedFlag { flag: 'v' } => {
                    DiagnosticCode::RegexUnicodeSetsFlagUnsupported
                }
                SyntaxError::UnsupportedFlag { .. } | SyntaxError::UnknownFlag { .. } => {
                    DiagnosticCode::RegexFlagUnsupported
                }
                SyntaxError::RepeatedFlag { .. } | SyntaxError::Pattern { .. } => {
                    DiagnosticCode::RegexInvalid
                }
            };
            Diagnostic::new(code, error.to_string(), span)
        })
}
