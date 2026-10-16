//! Kernel text: the one plain text notation for the kernel (`K-TEXT-001`).
//!
//! It is what the conformance corpus is written in and what a log, a diff or
//! a model is shown. [`print_document`] writes one canonical text per
//! document and [`parse_document`] reads it back to an equal document; the
//! same holds for definitions.

mod lexer;
mod parser;
mod printer;

use crate::validate::StatementForm;

pub use parser::{
    parse_definition, parse_definition_with_limits, parse_document, parse_document_with_limits,
};
pub use printer::{print_definition, print_document};

/// A kernel text that is not a document or a definition, and where.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason} (line {line}, column {column})")]
pub struct ParseError {
    /// The line of the offending token, from 1.
    pub line: u32,
    /// Its column in characters, from 1.
    pub column: u32,
    pub reason: ParseErrorReason,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseErrorReason {
    #[error("found {found}; expected {expected}")]
    Unexpected { found: String, expected: String },
    /// `join`, `yield` and `cancel` are the whole right-hand side of their
    /// own statement and cannot sit inside an expression (`K-EVAL-008`).
    #[error(
        "`{}` cannot sit inside an expression: make it the whole right-hand side of its own \
         statement and name its result",
        .form.keyword()
    )]
    StatementForm { form: StatementForm },
    /// An argument of a statement spelling of `join`, `cancel` or `sleep` is
    /// a variable or a literal.
    #[error(
        "an argument here is a variable or a literal; bind the computation to a variable first"
    )]
    ArgumentNotAtom,
    #[error("no `use` line names a library function `{name}`")]
    UnknownFunction { name: String },
    #[error("more than one `use` line names `{name}`; call it by identity, as `@<hex>(…)`")]
    AmbiguousFunction { name: String },
    #[error("{item} is given twice")]
    Duplicate { item: String },
    #[error("{item} is missing")]
    Missing { item: &'static str },
    #[error("nests deeper than {limit} levels; flatten the program")]
    TooDeep { limit: usize },
    #[error("{problem}")]
    Malformed { problem: String },
}

/// The words kernel text reserves. A name spelled like one is written
/// between backticks.
pub(crate) const KEYWORDS: &[&str] = &[
    "absent",
    "all",
    "any",
    "apply",
    "as",
    "await",
    "body",
    "break",
    "by_spelling",
    "call",
    "cancel",
    "catch",
    "charge",
    "clock",
    "continue",
    "do",
    "effect",
    "else",
    "entry",
    "errors",
    "fail",
    "false",
    "finally",
    "finish",
    "float",
    "fn",
    "for",
    "function",
    "guard",
    "if",
    "in",
    "inf",
    "invoke",
    "join",
    "kernel",
    "let",
    "main",
    "map",
    "nan",
    "native",
    "null",
    "numbers",
    "perform",
    "print",
    "private",
    "race",
    "random",
    "read",
    "remove",
    "result",
    "return",
    "set",
    "settled",
    "sleep",
    "spawn",
    "throw",
    "true",
    "try",
    "use",
    "while",
    "yield",
];

pub(crate) fn is_keyword(word: &str) -> bool {
    KEYWORDS.binary_search(&word).is_ok()
}
