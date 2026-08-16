use crate::{Diagnostic, DiagnosticCode, SourceSpan};

pub(crate) fn validate_literal(
    pattern: &str,
    flags: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    lashlang::validate_typescript_regexp(pattern, flags).map_err(|error| {
        let (code, message) = match error {
            lashlang::TypeScriptRegExpValidationError::PatternTooLong => (
                DiagnosticCode::RegexPatternTooLong,
                format!(
                    "regular-expression patterns may contain at most {} UTF-16 code units",
                    lashlang::TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS
                ),
            ),
            lashlang::TypeScriptRegExpValidationError::PatternTooDeep => (
                DiagnosticCode::RegexNestingLimit,
                format!(
                    "regular-expression group nesting may not exceed {} levels; split the pattern into smaller expressions",
                    lashlang::TYPESCRIPT_REGEXP_MAX_NESTING
                ),
            ),
            lashlang::TypeScriptRegExpValidationError::UnsupportedFlag('d') => (
                DiagnosticCode::RegexIndicesFlagUnsupported,
                "RegExp flag `d` is unsupported because match indices are not exposed; remove `d` and use match.index plus capture lengths".to_string(),
            ),
            lashlang::TypeScriptRegExpValidationError::UnsupportedFlag('v') => (
                DiagnosticCode::RegexUnicodeSetsFlagUnsupported,
                "RegExp flag `v` is unsupported; use `u` with ordinary Unicode character classes".to_string(),
            ),
            lashlang::TypeScriptRegExpValidationError::UnsupportedFlag(flag) => (
                DiagnosticCode::RegexFlagUnsupported,
                format!("RegExp flag `{flag}` is unsupported; use only g, i, m, s, u, or y"),
            ),
            lashlang::TypeScriptRegExpValidationError::InvalidFlags => (
                DiagnosticCode::RegexInvalid,
                "invalid or duplicate RegExp flags; use each of g, i, m, s, u, and y at most once".to_string(),
            ),
            lashlang::TypeScriptRegExpValidationError::InvalidPattern => (
                DiagnosticCode::RegexInvalid,
                "invalid ECMAScript regular-expression pattern".to_string(),
            ),
        };
        Diagnostic::new(code, message, span)
    })
}

pub(crate) fn validate_literal_shape(
    pattern: &str,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    lashlang::validate_typescript_regexp_shape(pattern).map_err(|error| {
        let (code, message) = match error {
            lashlang::TypeScriptRegExpValidationError::PatternTooLong => (
                DiagnosticCode::RegexPatternTooLong,
                format!(
                    "regular-expression patterns may contain at most {} UTF-16 code units",
                    lashlang::TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS
                ),
            ),
            lashlang::TypeScriptRegExpValidationError::PatternTooDeep => (
                DiagnosticCode::RegexNestingLimit,
                format!(
                    "regular-expression group nesting may not exceed {} levels; split the pattern into smaller expressions",
                    lashlang::TYPESCRIPT_REGEXP_MAX_NESTING
                ),
            ),
            _ => unreachable!("shape validation only returns size and nesting errors"),
        };
        Diagnostic::new(code, message, span)
    })
}
