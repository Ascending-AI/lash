//! Kernel text: the one plain text notation for the kernel (`K-TEXT-001`).
//!
//! It is what the conformance corpus is written in and what a log, a diff or
//! a model is shown. [`print_document`] writes one canonical text per
//! document and [`parse_document`] reads it back to an equal document; the
//! same holds for definitions.

mod lexer;
pub(crate) mod parser;
mod printer;

use crate::validate::StatementForm;

pub use parser::{
    parse_definition, parse_definition_with_limits, parse_definition_with_locals,
    parse_definition_with_locals_and_limits, parse_document, parse_document_with_limits,
    parse_signature,
};

pub use printer::{print_definition, print_document, print_document_with_ranges};

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
    /// An action appears where an atom-only spelling expects a variable
    /// or literal. Parentheses spell a computed action argument.
    #[error("`{}` is an action, not an atom; parenthesize an expression argument", .form.keyword())]
    StatementForm { form: StatementForm },
    /// An argument of a statement spelling of `join`, `cancel` or `sleep` is
    /// a variable or a literal.
    #[error(
        "an argument here is a variable or a literal; bind the computation to a variable first"
    )]
    ArgumentNotAtom,
    #[error("no `use` line names a library function `{name}`")]
    UnknownFunction { name: String },
    #[error("no operation declaration names `{name}`")]
    UnknownOperation { name: String },
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
