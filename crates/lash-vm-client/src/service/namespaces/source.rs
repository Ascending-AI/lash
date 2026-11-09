//! The dialect token streams decide identifier spelling and exclude trivia.
use std::collections::BTreeSet;

pub(super) fn identifiers(source: &str, typescript: bool) -> Option<BTreeSet<String>> {
    if typescript {
        let names = lash_dialect_typescript::source_identifiers(source);
        (!names.contains("*")).then_some(names)
    } else {
        use ruff_python_ast::{PySourceType, token::TokenKind};
        use unicode_normalization::UnicodeNormalization as _;

        // Ruff exposes token ranges through its parsed source. In particular,
        // f-string expressions are tokens, while their literal parts are not names.
        let parsed = ruff_python_parser::parse_unchecked_source(source, PySourceType::Python);
        // A lexical error can hide names inside an unreadable token.
        if parsed
            .errors()
            .iter()
            .any(|error| matches!(error.error, ruff_python_parser::ParseErrorType::Lexical(_)))
        {
            return None;
        }
        Some(
            parsed
                .tokens()
                .iter()
                .filter(|token| token.kind() == TokenKind::Name)
                .map(|token| {
                    let (_, range) = token.as_tuple();
                    source[std::ops::Range::<usize>::from(range)]
                        .nfkc()
                        .collect()
                })
                .collect(),
        )
    }
}
