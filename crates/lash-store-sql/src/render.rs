//! The startup renderer: neutral SQL to one backend's exact text.
//!
//! This is a tokenizer, not a regex. It walks the statement once, knows where
//! a string literal, a quoted identifier and a comment begin and end, and
//! rewrites only the tokens that are genuinely a placeholder, a table name or
//! a vocabulary token. A regex over the same text rewrites a `?` inside
//! `'why?'`, a `$1` inside a comment, and the `await_event_waits` inside
//! `await_event_waits_archive`; each of those is a test in this module.

use std::fmt;

/// One term of the domain vocabulary a backend supplies at render time.
///
/// `expand` is the function that spells the term for one column — exactly the
/// `lash_core::store_backend_support::*_predicate_sql` shape. The expansions
/// live in the backend crate because this crate has no `lash-core` dependency
/// and the vocabulary has exactly one source; see
/// [`Vocabulary`] for how a backend registers them.
#[derive(Clone, Copy)]
pub struct VocabularyTerm {
    name: &'static str,
    expand: fn(&str) -> String,
}

impl VocabularyTerm {
    /// Name one vocabulary term and the function that spells it.
    #[must_use]
    pub const fn new(name: &'static str, expand: fn(&str) -> String) -> Self {
        Self { name, expand }
    }

    /// The term's name, as a neutral statement spells it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Debug for VocabularyTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VocabularyTerm")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PartialEq for VocabularyTerm {
    /// Two terms are the same term when they answer to the same name: the
    /// expansion is a function pointer, and comparing those says nothing
    /// useful about whether two vocabularies agree.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for VocabularyTerm {}

/// The domain vocabulary a dialect renders with.
///
/// This is the third render axis, beside the placeholder style and the table
/// prefix. A neutral statement names a vocabulary predicate as a token —
/// `{{live_process_status(status)}}` — and the renderer replaces it, once, at
/// startup, with whatever the supplied term spells for that column.
///
/// It is **not** a template mechanism for dialect forks. A term names domain
/// vocabulary that both backends spell identically and that is generated from
/// one source elsewhere; a statement whose text forks between the backends is
/// still two statements with two owners and a manifest entry each
/// (ADR 0098).
///
/// A backend registers its expansions once:
///
/// ```ignore
/// use lash_store_sql::{Vocabulary, VocabularyTerm};
/// use lash_core::store_backend_support as vocabulary;
///
/// const PROCESS_LIFECYCLE: Vocabulary = Vocabulary::new(&[
///     VocabularyTerm::new(
///         "live_process_status",
///         vocabulary::live_process_status_predicate_sql,
///     ),
///     VocabularyTerm::new(
///         "retired_process_status",
///         vocabulary::retired_process_status_predicate_sql,
///     ),
/// ]);
///
/// let dialect = Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE);
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Vocabulary {
    terms: &'static [VocabularyTerm],
}

impl Vocabulary {
    /// No vocabulary at all: every token is a refusal.
    ///
    /// This is what a dialect carries until a backend attaches one, so a
    /// statement that uses a token under a dialect nobody supplied fails at
    /// startup naming the term rather than reaching a database.
    pub const EMPTY: Self = Self { terms: &[] };

    /// Register the terms a dialect may expand.
    #[must_use]
    pub const fn new(terms: &'static [VocabularyTerm]) -> Self {
        Self { terms }
    }

    /// Whether this vocabulary supplies nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Every registered term name, in declaration order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.terms.iter().map(VocabularyTerm::name).collect()
    }

    fn expand(&self, name: &str, column: &str) -> Option<String> {
        self.terms
            .iter()
            .find(|term| term.name == name)
            .map(|term| (term.expand)(column))
    }
}

impl fmt::Debug for Vocabulary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// One database a deployment reaches, and the tables it holds.
///
/// `qualifier` is the name SQL addresses that database by on the connection
/// being rendered for — `main` for the connection's own file, or the name it
/// was `ATTACH`ed under. `tables` are the unprefixed table names that database
/// carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaTables {
    qualifier: &'static str,
    tables: &'static [&'static str],
}

impl SchemaTables {
    /// Name one database and the tables it holds.
    #[must_use]
    pub const fn new(qualifier: &'static str, tables: &'static [&'static str]) -> Self {
        Self { qualifier, tables }
    }

    /// The name SQL addresses this database by.
    #[must_use]
    pub const fn qualifier(&self) -> &'static str {
        self.qualifier
    }
}

/// Where one deployment layout puts each table a statement can name.
///
/// This is what makes the schema a property of the **table** rather than of
/// the statement (FIG-3406). A SQLite connection can reach two databases at
/// once — the session catalog as `main` and a bound process registry as
/// `process_registry` — and a statement that joins them needs a different
/// qualifier per table, which a single per-statement schema cannot express.
///
/// Resolution is by first match, in declaration order. One table really does
/// live in two databases (`effect_scope_retirements` is carried by both the
/// effect journal and a bound process registry, ADR 0049); a layout places it
/// in exactly one of them, and the other copy is reached through a different
/// layout rather than through a second entry here.
///
/// A table no entry places is a render refusal, so a statement a connection
/// must not issue cannot be rendered for that connection's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableLayout {
    schemas: &'static [SchemaTables],
}

impl TableLayout {
    /// Declare where each database's tables live, in resolution order.
    #[must_use]
    pub const fn new(schemas: &'static [SchemaTables]) -> Self {
        Self { schemas }
    }

    /// The qualifier `table` is addressed through, or `None` when this layout
    /// does not place it.
    #[must_use]
    pub fn qualifier_for(&self, table: &str) -> Option<&'static str> {
        self.schemas
            .iter()
            .find(|schema| schema.tables.contains(&table))
            .map(|schema| schema.qualifier)
    }

    /// Every database this layout reaches, in declaration order.
    #[must_use]
    pub fn qualifiers(&self) -> Vec<&'static str> {
        self.schemas.iter().map(SchemaTables::qualifier).collect()
    }
}

/// How a backend spells a bound parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placeholder {
    /// `?1`, `?2`, … — rusqlite.
    Question,
    /// `$1`, `$2`, … — sqlx/PostgreSQL.
    Dollar,
}

/// Everything that differs between two backends' spelling of the same
/// statement: how a parameter is written, how a table is addressed, and which
/// domain vocabulary its tokens expand from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    placeholder: Placeholder,
    /// Prepended to every table name. `lash_` on PostgreSQL, empty on SQLite.
    table_prefix: &'static str,
    /// Where this deployment puts each table, `None` when no table is
    /// qualified at all. SQLite reaches the effect journal through `main` on
    /// its own connection and through an `ATTACH`ed name from the retention
    /// sweep, and reaches a bound process registry beside either of them, so
    /// the qualifier is resolved per table from the layout rather than
    /// `format!`ed at every call site.
    layout: Option<TableLayout>,
    /// The terms `{{term(column)}}` tokens expand from. Empty until a backend
    /// attaches one, because this crate has no source for the vocabulary.
    vocabulary: Vocabulary,
}

impl Dialect {
    /// The SQLite dialect, addressing each table through the database
    /// `layout` places it in.
    ///
    /// A table the layout does not place is a render refusal
    /// ([`RenderError::TableNotPlaced`]), which is how a statement that can
    /// only be issued on a connection with a process registry attached fails
    /// to render for the layout that has none.
    #[must_use]
    pub const fn sqlite(layout: TableLayout) -> Self {
        Self {
            placeholder: Placeholder::Question,
            table_prefix: "",
            layout: Some(layout),
            vocabulary: Vocabulary::EMPTY,
        }
    }

    /// The SQLite dialect, addressing tables with no schema qualifier.
    ///
    /// Qualifiers exist because the effect journal's tables are reached
    /// through an `ATTACH`ed name as well as through `main`. A table family
    /// that lives on one connection only — the process registry's own
    /// database — is addressed the way it always has been, unqualified, so
    /// that its rendered text is what its `INDEXED BY` plans were measured
    /// against.
    #[must_use]
    pub const fn sqlite_unqualified() -> Self {
        Self {
            placeholder: Placeholder::Question,
            table_prefix: "",
            layout: None,
            vocabulary: Vocabulary::EMPTY,
        }
    }

    /// The PostgreSQL dialect. Tables carry the `lash_` prefix that every
    /// existing database on this tier was provisioned with, and there is one
    /// database, so no table is qualified.
    #[must_use]
    pub const fn postgres() -> Self {
        Self {
            placeholder: Placeholder::Dollar,
            table_prefix: "lash_",
            layout: None,
            vocabulary: Vocabulary::EMPTY,
        }
    }

    /// Attach the vocabulary this dialect's tokens expand from.
    ///
    /// A family whose statements use vocabulary tokens attaches the backend's
    /// vocabulary once, where it renders its statement set. A family that uses
    /// no token needs none: a dialect with no vocabulary refuses every token
    /// rather than rendering an empty predicate.
    #[must_use]
    pub const fn with_vocabulary(mut self, vocabulary: Vocabulary) -> Self {
        self.vocabulary = vocabulary;
        self
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
    /// A table this crate owns that the dialect's layout does not place in
    /// any database the connection reaches.
    TableNotPlaced {
        /// The table the statement named.
        name: String,
        /// Byte offset of the identifier.
        at: usize,
        /// The databases the layout does reach.
        schemas: Vec<&'static str>,
    },
    /// A `{` or `}` that is not a well-formed `{{term(column)}}` token.
    MalformedVocabularyToken {
        /// Byte offset of the brace that opened the token.
        at: usize,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// A token whose column is neither a plain nor a qualified identifier.
    VocabularyColumnNotIdentifier {
        /// The text found where a column reference was expected.
        column: String,
        /// Byte offset of the token.
        at: usize,
    },
    /// A token under a dialect that carries no vocabulary at all.
    VocabularyNotSupplied {
        /// The term the statement named.
        name: String,
        /// Byte offset of the token.
        at: usize,
    },
    /// A token naming a term the supplied vocabulary does not define.
    UnknownVocabularyTerm {
        /// The term the statement named.
        name: String,
        /// Byte offset of the token.
        at: usize,
        /// The terms the dialect's vocabulary does define.
        known: Vec<&'static str>,
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
            Self::TableNotPlaced { name, at, schemas } => write!(
                f,
                "`{name}` at byte {at} is not placed by this deployment layout, which reaches \
                 {schemas:?}. A statement may only name tables the connection it is rendered \
                 for can reach; if this one belongs to a layout with more databases attached, \
                 render it for that layout."
            ),
            Self::MalformedVocabularyToken { at, reason } => write!(
                f,
                "byte {at}: {reason}; a vocabulary token is spelled \
                 `{{{{term(column)}}}}`"
            ),
            Self::VocabularyColumnNotIdentifier { column, at } => write!(
                f,
                "the vocabulary token at byte {at} carries `{column}` where a column reference \
                 belongs; it must be a plain (`status`) or qualified (`p.status`) identifier"
            ),
            Self::VocabularyNotSupplied { name, at } => write!(
                f,
                "the vocabulary token `{name}` at byte {at} has no expansion: this dialect was \
                 rendered without a vocabulary. The backend crate attaches one with \
                 `Dialect::with_vocabulary`."
            ),
            Self::UnknownVocabularyTerm { name, at, known } => write!(
                f,
                "the vocabulary token `{name}` at byte {at} is not a term this dialect's \
                 vocabulary defines; it defines {known:?}"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// Render `neutral` into `dialect`'s exact text.
///
/// `tables` is the set of table names that may appear; every occurrence of one
/// as a whole token is rewritten, and a table position naming anything else is
/// refused. A dialect carrying a [`TableLayout`] resolves each of those names
/// to the database that layout places it in, so one statement can address two
/// databases; a table the layout does not place is refused rather than
/// rendered unqualified. `{{term(column)}}` tokens expand from the vocabulary
/// the dialect carries, once, here — a token inside a string literal or a
/// comment is that literal's or comment's own text and survives verbatim.
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
    // Relation names the statement binds for itself: the `scope` of
    // `WITH scope AS (…)`. They are not tables, they carry no prefix and no
    // schema qualifier, and a later `FROM scope` must be left alone rather
    // than refused. A name is bound before it can be referenced, so one
    // forward pass sees every binding in time.
    let mut local_relations: Vec<&str> = Vec::new();
    // The identifier immediately before the current one, with nothing but
    // whitespace between them. An upsert's `DO UPDATE SET` spells `UPDATE`
    // where no table follows, so the keyword alone cannot decide.
    let mut previous_word: Option<&str> = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\'' => {
                index = copy_delimited(neutral, index, b'\'', true, "string literal", &mut out)?;
                expect_table = None;
                previous_word = None;
            }
            b'"' => {
                index = copy_delimited(neutral, index, b'"', true, "quoted identifier", &mut out)?;
                expect_table = None;
                previous_word = None;
            }
            b'`' => {
                index = copy_delimited(neutral, index, b'`', false, "quoted identifier", &mut out)?;
                expect_table = None;
                previous_word = None;
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
                previous_word = None;
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
                previous_word = None;
            }
            b'$' => return Err(RenderError::DollarPlaceholder { at: index }),
            b'{' => {
                index = expand_vocabulary_token(neutral, index, dialect.vocabulary, &mut out)?;
                expect_table = None;
            }
            b'}' => {
                return Err(RenderError::MalformedVocabularyToken {
                    at: index,
                    reason: "a `}` outside a vocabulary token",
                });
            }
            _ if is_identifier_start(byte) => {
                let start = index;
                let mut end = index;
                while end < bytes.len() && is_identifier_byte(bytes[end]) {
                    end += 1;
                }
                let word = &neutral[start..end];
                let qualified = start > 0 && bytes[start - 1] == b'.';
                if !qualified && tables.contains(&word) {
                    if let Some(layout) = dialect.layout {
                        let Some(schema) = layout.qualifier_for(word) else {
                            return Err(RenderError::TableNotPlaced {
                                name: word.to_string(),
                                at: start,
                                schemas: layout.qualifiers(),
                            });
                        };
                        out.push_str(schema);
                        out.push('.');
                    }
                    out.push_str(dialect.table_prefix);
                    out.push_str(word);
                } else {
                    out.push_str(word);
                    if followed_by_as_open_paren(neutral, end) {
                        local_relations.push(word);
                    }
                    if let Some(position) = expect_table
                        && !qualified
                        && !local_relations.contains(&word)
                        && !(position == TablePosition::From
                            && followed_by_open_paren(neutral, end))
                    {
                        return Err(RenderError::UnknownTable {
                            name: word.to_string(),
                            at: start,
                        });
                    }
                }
                expect_table = table_position(word, previous_word);
                previous_word = Some(word);
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
                    previous_word = None;
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
///
/// `UPDATE` names a table except in an upsert's `DO UPDATE SET`, where it
/// names the conflicting row this statement already declared its table for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TablePosition {
    From,
    Other,
}

/// Whether `word` opens a table position, given the identifier before it.
///
/// `previous` decides two cases: an upsert's `ON CONFLICT … DO UPDATE SET`
/// writes `UPDATE` with no table after it, because the table is the one the
/// insert already named. Every other `UPDATE` is a statement head and does
/// take a table.
fn table_position(word: &str, previous: Option<&str>) -> Option<TablePosition> {
    if word.eq_ignore_ascii_case("from") {
        Some(TablePosition::From)
    } else if word.eq_ignore_ascii_case("update") {
        // `ON CONFLICT … DO UPDATE SET` writes the row the insert conflicted
        // with; `FOR UPDATE OF <alias>` names an alias. Neither takes a table.
        let names_no_relation = previous.is_some_and(|before| {
            before.eq_ignore_ascii_case("do") || before.eq_ignore_ascii_case("for")
        });
        (!names_no_relation).then_some(TablePosition::Other)
    } else if word.eq_ignore_ascii_case("into") || word.eq_ignore_ascii_case("join") {
        Some(TablePosition::Other)
    } else {
        None
    }
}

/// Expand one `{{term(column)}}` token that opens at `open`.
///
/// The expansion is written out verbatim and is **not** rescanned: it is the
/// vocabulary's own spelling of a predicate over a column, already complete,
/// and rescanning it would put quoted lifecycle labels back through the
/// placeholder and table passes for nothing.
fn expand_vocabulary_token(
    text: &str,
    open: usize,
    vocabulary: Vocabulary,
    out: &mut String,
) -> Result<usize, RenderError> {
    if text.as_bytes().get(open + 1) != Some(&b'{') {
        return Err(RenderError::MalformedVocabularyToken {
            at: open,
            reason: "a `{` that does not open a vocabulary token",
        });
    }
    let body_start = open + 2;
    let Some(offset) = text[body_start..].find("}}") else {
        return Err(RenderError::Unterminated {
            kind: "vocabulary token",
            at: open,
        });
    };
    let body = &text[body_start..body_start + offset];
    let end = body_start + offset + 2;

    let malformed = |reason| RenderError::MalformedVocabularyToken { at: open, reason };
    let Some(paren) = body.find('(') else {
        return Err(malformed(
            "a vocabulary token names one column in parentheses",
        ));
    };
    let name = body[..paren].trim();
    let arguments = &body[paren + 1..];
    let Some(close) = arguments.rfind(')') else {
        return Err(malformed("a vocabulary token's column list is not closed"));
    };
    if !arguments[close + 1..].trim().is_empty() {
        return Err(malformed(
            "a vocabulary token ends at its closing parenthesis",
        ));
    }
    if !is_plain_identifier(name) {
        return Err(malformed("a vocabulary term is a plain identifier"));
    }
    let column = arguments[..close].trim();
    if !is_column_reference(column) {
        return Err(RenderError::VocabularyColumnNotIdentifier {
            column: column.to_string(),
            at: open,
        });
    }

    let Some(expansion) = vocabulary.expand(name, column) else {
        return Err(if vocabulary.is_empty() {
            RenderError::VocabularyNotSupplied {
                name: name.to_string(),
                at: open,
            }
        } else {
            RenderError::UnknownVocabularyTerm {
                name: name.to_string(),
                at: open,
                known: vocabulary.names(),
            }
        });
    };
    out.push_str(&expansion);
    Ok(end)
}

/// `status`: one unquoted SQL identifier.
fn is_plain_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(is_identifier_start) && bytes.all(is_identifier_byte)
}

/// `status` or `processes.status`: what the vocabulary helpers take.
fn is_column_reference(text: &str) -> bool {
    let mut parts = text.split('.');
    let first = parts.next().is_some_and(is_plain_identifier);
    match parts.next() {
        None => first,
        Some(second) => first && is_plain_identifier(second) && parts.next().is_none(),
    }
}

fn followed_by_open_paren(text: &str, from: usize) -> bool {
    text[from..]
        .trim_start_matches(|character: char| character.is_ascii_whitespace())
        .starts_with('(')
}

/// Whether the identifier that ends at `from` is followed by `AS (` — the one
/// shape that binds a relation name inside a statement: `WITH scope AS (…)`,
/// and a derived table's `) AS scope`. A column alias (`COUNT(*) AS n,`) is not
/// followed by a parenthesis, so it never registers.
fn followed_by_as_open_paren(text: &str, from: usize) -> bool {
    let rest = text[from..].trim_start_matches(|character: char| character.is_ascii_whitespace());
    let Some(after_as) = rest.get(..2) else {
        return false;
    };
    if !after_as.eq_ignore_ascii_case("as") {
        return false;
    }
    let tail = &rest[2..];
    if tail
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return false;
    }
    let mut tail = tail.trim_start_matches(|character: char| character.is_ascii_whitespace());
    // PostgreSQL lets a common table expression state its inlining:
    // `AS MATERIALIZED (`, `AS NOT MATERIALIZED (`. It is still a binding.
    for hint in ["NOT", "MATERIALIZED"] {
        if tail.len() >= hint.len()
            && tail[..hint.len()].eq_ignore_ascii_case(hint)
            && !tail
                .as_bytes()
                .get(hint.len())
                .copied()
                .is_some_and(is_identifier_byte)
        {
            tail = tail[hint.len()..]
                .trim_start_matches(|character: char| character.is_ascii_whitespace());
        }
    }
    tail.starts_with('(')
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
