//! The startup renderer: neutral SQL to one backend's exact text.
//!
//! This is a tokenizer, not a regex. It walks the statement once, knows where
//! a string literal, a quoted identifier and a comment begin and end, and
//! rewrites only the tokens that are genuinely a placeholder or a table name.
//! A regex over the same text rewrites a `?` inside `'why?'`, a `$1` inside a
//! comment, and the `await_event_waits` inside `await_event_waits_archive`;
//! each of those is a test in this module.

use std::fmt;

/// How a backend spells a bound parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placeholder {
    /// `?1`, `?2`, … — rusqlite.
    Question,
    /// `$1`, `$2`, … — sqlx/PostgreSQL.
    Dollar,
}

/// Everything that differs between two backends' spelling of the same
/// statement: how a parameter is written, and how a table is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    placeholder: Placeholder,
    /// Prepended to every table name. `lash_` on PostgreSQL, empty on SQLite.
    table_prefix: &'static str,
    /// Database qualifier written before the table name, `None` for no
    /// qualifier. SQLite reaches the same tables through `main` on the
    /// journal's own connection and through an `ATTACH`ed name from the
    /// retention sweep, so the qualifier is a render parameter rather than a
    /// `format!` at every call site.
    schema: Option<&'static str>,
}

impl Dialect {
    /// The SQLite dialect, addressing tables through `schema`.
    #[must_use]
    pub const fn sqlite(schema: &'static str) -> Self {
        Self {
            placeholder: Placeholder::Question,
            table_prefix: "",
            schema: Some(schema),
        }
    }

    /// The PostgreSQL dialect. Tables carry the `lash_` prefix that every
    /// existing database on this tier was provisioned with.
    #[must_use]
    pub const fn postgres() -> Self {
        Self {
            placeholder: Placeholder::Dollar,
            table_prefix: "lash_",
            schema: None,
        }
    }
}

/// A defect in a statement's neutral text, reported at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// A `?` that is not followed by a parameter number.
    UnnumberedPlaceholder {
        /// Byte offset of the `?`.
        at: usize,
    },
    /// A `$` in neutral text. Neutral form spells parameters `?N`.
    DollarPlaceholder {
        /// Byte offset of the `$`.
        at: usize,
    },
    /// A string literal, quoted identifier or block comment with no end.
    Unterminated {
        /// What was left open.
        kind: &'static str,
        /// Byte offset where it opened.
        at: usize,
    },
    /// A table position naming something this crate does not own.
    UnknownTable {
        /// The identifier found where a table was expected.
        name: String,
        /// Byte offset of the identifier.
        at: usize,
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnnumberedPlaceholder { at } => write!(
                f,
                "neutral SQL spells parameters `?N`; found a bare `?` at byte {at}"
            ),
            Self::DollarPlaceholder { at } => write!(
                f,
                "neutral SQL spells parameters `?N`, never `$N`; found `$` at byte {at}"
            ),
            Self::Unterminated { kind, at } => {
                write!(f, "unterminated {kind} opened at byte {at}")
            }
            Self::UnknownTable { name, at } => write!(
                f,
                "`{name}` at byte {at} is in a table position but is not a table \
                 `lash-store-sql` owns; add it to `TABLES` or spell the statement elsewhere"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// Render `neutral` into `dialect`'s exact text.
///
/// `tables` is the set of table names that may appear; every occurrence of one
/// as a whole token is rewritten, and a table position naming anything else is
/// refused.
///
/// # Errors
///
/// See [`RenderError`].
pub fn render(neutral: &str, dialect: Dialect, tables: &[&str]) -> Result<String, RenderError> {
    let bytes = neutral.as_bytes();
    let mut out = String::with_capacity(neutral.len() + 16);
    let mut index = 0usize;
    // Set by FROM/INTO/UPDATE/JOIN; the next token has to be a table this
    // crate owns (or, after FROM, a parenthesised subquery or a table-valued
    // function).
    let mut expect_table: Option<TablePosition> = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\'' => {
                index = copy_delimited(neutral, index, b'\'', true, "string literal", &mut out)?;
                expect_table = None;
            }
            b'"' => {
                index = copy_delimited(neutral, index, b'"', true, "quoted identifier", &mut out)?;
                expect_table = None;
            }
            b'`' => {
                index = copy_delimited(neutral, index, b'`', false, "quoted identifier", &mut out)?;
                expect_table = None;
            }
            b'[' => {
                index = copy_until(
                    neutral,
                    index + 1,
                    b']',
                    "bracketed identifier",
                    &mut out,
                    "[",
                )?;
                expect_table = None;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                let end = neutral[index..]
                    .find('\n')
                    .map_or(bytes.len(), |offset| index + offset);
                out.push_str(&neutral[index..end]);
                index = end;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let Some(offset) = neutral[index + 2..].find("*/") else {
                    return Err(RenderError::Unterminated {
                        kind: "block comment",
                        at: index,
                    });
                };
                let end = index + 2 + offset + 2;
                out.push_str(&neutral[index..end]);
                index = end;
            }
            b'?' => {
                let start = index + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end == start {
                    return Err(RenderError::UnnumberedPlaceholder { at: index });
                }
                match dialect.placeholder {
                    Placeholder::Question => out.push('?'),
                    Placeholder::Dollar => out.push('$'),
                }
                out.push_str(&neutral[start..end]);
                index = end;
                expect_table = None;
            }
            b'$' => return Err(RenderError::DollarPlaceholder { at: index }),
            _ if is_identifier_start(byte) => {
                let start = index;
                let mut end = index;
                while end < bytes.len() && is_identifier_byte(bytes[end]) {
                    end += 1;
                }
                let word = &neutral[start..end];
                let qualified = start > 0 && bytes[start - 1] == b'.';
                if !qualified && tables.contains(&word) {
                    if let Some(schema) = dialect.schema {
                        out.push_str(schema);
                        out.push('.');
                    }
                    out.push_str(dialect.table_prefix);
                    out.push_str(word);
                } else {
                    out.push_str(word);
                    if let Some(position) = expect_table
                        && !qualified
                        && !(position == TablePosition::From
                            && followed_by_open_paren(neutral, end))
                    {
                        return Err(RenderError::UnknownTable {
                            name: word.to_string(),
                            at: start,
                        });
                    }
                }
                expect_table = table_position(word);
                index = end;
            }
            _ => {
                let Some(character) = neutral[index..].chars().next() else {
                    break;
                };
                out.push(character);
                index += character.len_utf8();
                if !character.is_whitespace() {
                    expect_table = None;
                }
            }
        }
    }
    Ok(out)
}

/// The keyword that put the renderer in a table position. `FROM` is the one
/// that also introduces subqueries (`FROM (SELECT …)`), table-valued functions
/// (`FROM json_each(…)`) and `EXTRACT(EPOCH FROM clock_timestamp())`, so only
/// it tolerates a following `(`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TablePosition {
    From,
    Other,
}

fn table_position(word: &str) -> Option<TablePosition> {
    if word.eq_ignore_ascii_case("from") {
        Some(TablePosition::From)
    } else if word.eq_ignore_ascii_case("into")
        || word.eq_ignore_ascii_case("update")
        || word.eq_ignore_ascii_case("join")
    {
        Some(TablePosition::Other)
    } else {
        None
    }
}

fn followed_by_open_paren(text: &str, from: usize) -> bool {
    text[from..]
        .trim_start_matches(|character: char| character.is_ascii_whitespace())
        .starts_with('(')
}

const fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Copy a delimited run that starts at `open` with the delimiter byte,
/// including both delimiters. When `doubled_escapes`, a doubled delimiter is
/// an escaped one and does not close the run.
fn copy_delimited(
    text: &str,
    open: usize,
    delimiter: u8,
    doubled_escapes: bool,
    kind: &'static str,
    out: &mut String,
) -> Result<usize, RenderError> {
    let bytes = text.as_bytes();
    let mut index = open + 1;
    loop {
        if index >= bytes.len() {
            return Err(RenderError::Unterminated { kind, at: open });
        }
        if bytes[index] == delimiter {
            if doubled_escapes && bytes.get(index + 1) == Some(&delimiter) {
                index += 2;
                continue;
            }
            index += 1;
            break;
        }
        index += 1;
    }
    out.push_str(&text[open..index]);
    Ok(index)
}

/// Copy from the opening byte through `close`, inclusive.
fn copy_until(
    text: &str,
    body: usize,
    close: u8,
    kind: &'static str,
    out: &mut String,
    opener: &str,
) -> Result<usize, RenderError> {
    let bytes = text.as_bytes();
    let open = body - opener.len();
    let mut index = body;
    while index < bytes.len() && bytes[index] != close {
        index += 1;
    }
    if index >= bytes.len() {
        return Err(RenderError::Unterminated { kind, at: open });
    }
    out.push_str(&text[open..=index]);
    Ok(index + 1)
}

#[cfg(test)]
mod tests;
