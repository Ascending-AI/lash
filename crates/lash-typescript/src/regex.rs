use crate::{Diagnostic, DiagnosticCode, SourceSpan};

pub(crate) fn validate_literal(
    pattern: &str,
    flags: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    lash_vm::validate_regexp(pattern, flags).map_err(|error| {
        let (code, message, repair) = match error {
            lash_vm::RegExpValidationError::PatternTooLong => (
                DiagnosticCode::RegexPatternTooLong,
                format!(
                    "regular-expression patterns may contain at most {} UTF-16 code units",
                    lash_vm::REGEXP_MAX_PATTERN_CODE_UNITS
                ),
                "shorten the pattern, or match in smaller steps",
            ),
            lash_vm::RegExpValidationError::PatternTooDeep => (
                DiagnosticCode::RegexNestingLimit,
                format!(
                    "regular-expression group nesting may not exceed {} levels",
                    lash_vm::REGEXP_MAX_NESTING
                ),
                "split the pattern into smaller expressions",
            ),
            lash_vm::RegExpValidationError::UnsupportedFlag('d') => (
                DiagnosticCode::RegexIndicesFlagUnsupported,
                "RegExp flag `d` is unsupported because match indices are not exposed".to_string(),
                "remove `d` and use match.index plus capture lengths",
            ),
            lash_vm::RegExpValidationError::UnsupportedFlag('v') => (
                DiagnosticCode::RegexUnicodeSetsFlagUnsupported,
                "RegExp flag `v` is unsupported".to_string(),
                "use `u` with ordinary Unicode character classes",
            ),
            lash_vm::RegExpValidationError::UnsupportedFlag(flag) => (
                DiagnosticCode::RegexFlagUnsupported,
                format!("RegExp flag `{flag}` is unsupported"),
                "use only the g, i, m, s, u, and y flags",
            ),
            lash_vm::RegExpValidationError::InvalidFlags => (
                DiagnosticCode::RegexInvalid,
                "invalid or duplicate RegExp flags".to_string(),
                "use each of g, i, m, s, u, and y at most once",
            ),
            lash_vm::RegExpValidationError::InvalidPattern => (
                DiagnosticCode::RegexInvalid,
                "invalid ECMAScript regular-expression pattern".to_string(),
                "escape any literal `(`, `)`, `[`, `]`, `{`, `}`, `*`, `+`, or `?`",
            ),
        };
        Diagnostic::with_repair(code, message, repair, span)
    })
}

pub(crate) fn validate_literal_shape(
    pattern: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    lash_vm::validate_regexp_shape(pattern).map_err(|error| {
        let (code, message, repair) = match error {
            lash_vm::RegExpValidationError::PatternTooLong => (
                DiagnosticCode::RegexPatternTooLong,
                format!(
                    "regular-expression patterns may contain at most {} UTF-16 code units",
                    lash_vm::REGEXP_MAX_PATTERN_CODE_UNITS
                ),
                "shorten the pattern, or match in smaller steps",
            ),
            lash_vm::RegExpValidationError::PatternTooDeep => (
                DiagnosticCode::RegexNestingLimit,
                format!(
                    "regular-expression group nesting may not exceed {} levels",
                    lash_vm::REGEXP_MAX_NESTING
                ),
                "split the pattern into smaller expressions",
            ),
            _ => unreachable!("shape validation only returns size and nesting errors"),
        };
        Diagnostic::with_repair(code, message, repair, span)
    })
}
