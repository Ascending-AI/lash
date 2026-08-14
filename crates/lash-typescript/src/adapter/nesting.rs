//! The source-nesting preflight.

use super::{Diagnostic, DiagnosticCode, MAX_SOURCE_NESTING_DEPTH, SourceSpan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanMode {
    Code,
    SingleQuoted,
    DoubleQuoted,
    Template,
    LineComment,
    BlockComment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceDelimiter {
    Paren,
    Bracket,
    /// A brace in expression position: an object literal or an object type.
    Brace,
    /// A brace that opens a statement block, which ends the statement forms
    /// that introduced it.
    StatementBrace,
    TemplateExpression,
}

#[derive(Clone, Copy, Debug)]
struct SourceNestingFrame {
    delimiter: SourceDelimiter,
    outer_operators: usize,
    /// A tail applied to an existing expression — `f(1)`, `a[0]` — rather than a
    /// fresh operand. Closing one leaves the tree one level deeper, so unlike an
    /// ordinary delimiter it keeps costing a unit after the pair closes.
    postfix: bool,
}

/// The last significant token. It decides whether a `{` opens an object literal
/// or a statement block, whether a `(`/`[`/`` ` `` is a postfix tail on an
/// existing expression, and whether a newline can end a statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviousToken {
    None,
    Byte(u8),
    /// A word after which a `{` can only open an object literal. `return` is the
    /// only one that can also end a statement, so the two bits are independent.
    Word {
        opens_expression: bool,
        ends_statement: bool,
        ends_expression: bool,
    },
}

impl PreviousToken {
    fn word(word: &str) -> Self {
        Self::Word {
            opens_expression: is_expression_prefix_word(word),
            ends_statement: word_can_end_statement(word),
            ends_expression: word_can_end_expression(word),
        }
    }

    fn opens_expression_brace(self) -> bool {
        match self {
            Self::Word {
                opens_expression, ..
            } => opens_expression,
            Self::Byte(byte) => matches!(
                byte,
                b'=' | b'('
                    | b','
                    | b'['
                    | b':'
                    | b'?'
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'<'
                    | b'>'
                    | b'!'
                    | b'~'
                    | b'&'
                    | b'|'
                    | b'^'
            ),
            Self::None => false,
        }
    }

    /// Whether this token can be the last one of a statement, which is the
    /// precondition for automatic semicolon insertion at a following newline.
    /// A keyword that opens an operand — `typeof`, `void`, `new`, `delete` —
    /// cannot, and treating it as if it could releases the budget on a shape
    /// that is still recursing.
    fn can_end_statement(self) -> bool {
        match self {
            Self::Word { ends_statement, .. } => ends_statement,
            Self::Byte(byte) => matches!(byte, b')' | b']' | b'}') || byte.is_ascii_alphanumeric(),
            Self::None => false,
        }
    }

    /// Whether this token can end an expression, which is what makes a
    /// following `(`, `[` or `` ` `` a postfix tail rather than a fresh operand.
    fn can_end_expression(self) -> bool {
        match self {
            Self::Word {
                ends_expression, ..
            } => ends_expression,
            Self::Byte(byte) => matches!(byte, b')' | b']' | b'}') || byte.is_ascii_alphanumeric(),
            Self::None => false,
        }
    }
}

/// Whether a word can end an expression, which is what makes a following `(`
/// or `[` a call or subscript rather than a fresh grouping. `if (…)` is a
/// statement head, not a call on `if`.
fn word_can_end_expression(word: &str) -> bool {
    word_can_end_statement(word) && !matches!(word, "return" | "break" | "continue")
}

/// Reserved words that cannot end a statement, so a newline after one is never
/// an automatic-semicolon-insertion boundary. Anything else — an identifier, a
/// literal, `return`, `break`, `continue` — can.
fn word_can_end_statement(word: &str) -> bool {
    !matches!(
        word,
        "as" | "asserts"
            | "await"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "finally"
            | "function"
            | "if"
            | "import"
            | "in"
            | "infer"
            | "instanceof"
            | "interface"
            | "is"
            | "keyof"
            | "let"
            | "new"
            | "of"
            | "readonly"
            | "satisfies"
            | "switch"
            | "throw"
            | "try"
            | "type"
            | "typeof"
            | "unique"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

/// Whether a token can continue the expression on the previous line, which is
/// what suppresses automatic semicolon insertion. Erring towards "continues"
/// only keeps the budget accumulating, which is the safe direction.
fn continues_previous_statement(byte: u8, word: Option<&str>) -> bool {
    if let Some(word) = word {
        // A cast keyword continues the expression on the previous line, exactly
        // as a relational keyword does.
        return matches!(word, "as" | "in" | "instanceof" | "of" | "satisfies");
    }
    matches!(
        byte,
        // A backtick continues the previous line as a tagged template.
        b'`' | b'.'
            | b','
            | b')'
            | b']'
            | b'}'
            | b';'
            | b':'
            | b'?'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'='
            | b'&'
            | b'|'
            | b'^'
            | b'('
            | b'['
    )
}

// Bound the source's nesting before SWC's recursive-descent parser sees it.
//
// SWC overflows the native stack on deep input and aborts the process rather
// than returning an error, so this scan has to be complete: any recursive
// production it fails to charge is a shape that takes the host down.
//
// **The grammar this argues over is the one SWC parses — all of TypeScript —
// not the subset the dialect accepts.** That distinction is the whole point.
// The dialect's rejections happen in the adapter and the lowerer, both of which
// run *after* a successful parse, so a production this crate will later refuse
// still recurses in the parser here. Labelled statements, `as` casts and the
// type-level operators are all outside the accepted surface and all of them
// recurse; each one was an abort until it was charged.
//
// A production can nest without bound exactly when its right-hand side can
// contain the non-terminal it defines. Walking SWC's grammar — equivalently,
// the `swc_ecma_ast` node kinds that can contain themselves, which
// `tests/grammar_coverage.rs` enumerates with a compiler-checked exhaustive
// match — every such production is one of:
//
//  1. Prefix — `UnaryExpression := <op> UnaryExpression`. One unit per operator
//     token: punctuation (`!`, `~`, unary `+`/`-`, `++`, `--`, `...`), value
//     keywords (`typeof`, `void`, `delete`, `new`, `await`, `yield`), and the
//     type-position keywords (`keyof`, `readonly`, `infer`, `unique`,
//     `asserts`, `is`), all listed by `is_recursive_operator_word` and
//     `is_recursive_operator_start`.
//  2. Infix — `BinaryExpression := Expression <op> Expression`, plus the
//     conditional, logical, arrow and assignment forms, plus the TypeScript
//     cast forms `Expression as Type` and `Expression satisfies Type`, and the
//     type-level `|`, `&` and `extends`. One unit per operator token, so a
//     left-nested chain costs its own length.
//  3. Postfix — `LeftHandSideExpression := LeftHandSideExpression <tail>`: a
//     call `(…)`, a subscript `[…]`, a member step `.a` / `?.a`, a tagged
//     template, a non-null `!`, or a type instantiation `<…>`. Member steps and
//     `!` are operator tokens and charge directly; the bracketed tails open
//     *and close* a delimiter pair, so their frame carries a `postfix` flag and
//     charges a unit when the pair closes. Without that a chain of any length
//     would sit at depth one.
//  4. Delimiter — every bracketed form: grouping, array and object literals and
//     patterns, class and function bodies, type literals and tuples, JSX
//     elements (through `<`), type argument lists, and a template hole `${…}`.
//     One unit while open. A template hole also charges a persistent unit,
//     because a template lowers into a left-nested concatenation chain whose
//     depth outlives the hole.
//  5. Statement form — a keyword-introduced statement that contains a
//     statement: `if`, `while`, `do`, `for`, `with`, and — the one that has no
//     keyword at all — `LabelledStatement := Identifier ':' Statement`, which
//     is charged on the `:` when no conditional is waiting for it and the
//     statement is at statement level.
//
// Nothing else recurses: identifiers, literals, keywords, JSX text and regular
// expression bodies are terminals; declarations, modules, classes and switch
// bodies recurse only through families 4 and 5; and the sequence expression is
// a flat list whose separator resets the run.
//
// `tests/depth_guard.rs` turns this list into the standing guard — every family
// and mixed combinations of them, repeated to 100 000 both inline and one per
// line, parsed in a child process on the 2 MiB stack contract.
// `tests/grammar_coverage.rs` cross-checks the list mechanically in two ways:
// an exhaustive match over SWC's own AST node kinds that fails to compile when
// SWC gains a variant, and a deterministic fuzzer that feeds random token
// sequences drawn from the charged alphabet through the preflight and into SWC
// inside a child process, where an abort is a test failure.
//
// The budget is one cumulative counter, not one per family: every unit above
// draws on the same 28, in this preflight and in the adapter's own conversion
// counter. Operator runs from an enclosing delimiter frame stay live while the
// scanner visits an inner expression. A statement boundary — `;`, `,`, the `}`
// that closes a statement block, or a newline in automatic-semicolon-insertion
// position — releases the operator run it terminates, so a flat sequence of
// statements stays one level deep whether or not it is punctuated. The ASI
// release is the delicate part: it must not fire where no statement can end
// (after a prefix keyword, or while a statement form is still open) and must
// not fire when the next token continues the expression — including a backtick,
// which makes the next line a tagged template, and `as`/`satisfies`, which make
// it a cast — or a newline-separated chain would slip through uncharged.
pub(super) fn guard_source_nesting(source: &str) -> Result<(), Diagnostic> {
    let bytes = source.as_bytes();
    let mut mode = ScanMode::Code;
    let mut escaped = false;
    let mut frames = Vec::new();
    let mut current_operators = 0usize;
    let mut previous = PreviousToken::None;
    // A newline that could end a statement; the following token decides whether
    // it actually did.
    let mut pending_statement_end = false;
    // Statement forms whose controlled statement has not been reached yet. A
    // newline cannot end a statement while one is open: `if (1)` is not a
    // complete statement however the line breaks.
    let mut open_statement_forms = 0usize;
    // Conditional operators awaiting their `:`. A `:` that no `?` is waiting for
    // is a label or an annotation, not a ternary arm.
    let mut open_conditionals = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        match mode {
            ScanMode::Code => {
                // `None` leaves the previous significant token in place, so
                // whitespace and comments never change how a `{` is classified.
                let mut scanned = Some(PreviousToken::Byte(byte));
                if pending_statement_end && !byte.is_ascii_whitespace() {
                    let word = if is_identifier_start(byte) {
                        let mut end = index;
                        while bytes
                            .get(end + 1)
                            .is_some_and(|byte| is_identifier_continue(*byte))
                        {
                            end += 1;
                        }
                        Some(&source[index..=end])
                    } else {
                        None
                    };
                    let comment = byte == b'/' && matches!(next, Some(b'/') | Some(b'*'));
                    if !comment {
                        pending_statement_end = false;
                        if !continues_previous_statement(byte, word) {
                            current_operators = 0;
                            open_statement_forms = 0;
                            open_conditionals = 0;
                        }
                    }
                }
                match (byte, next) {
                    (b'/', Some(b'/')) => {
                        mode = ScanMode::LineComment;
                        index += 1;
                        scanned = None;
                    }
                    (b'/', Some(b'*')) => {
                        mode = ScanMode::BlockComment;
                        index += 1;
                        scanned = None;
                    }
                    (b'\'', _) => {
                        mode = ScanMode::SingleQuoted;
                        escaped = false;
                    }
                    (b'"', _) => {
                        mode = ScanMode::DoubleQuoted;
                        escaped = false;
                    }
                    (b'`', _) => {
                        // A tagged template is a postfix tail; charge it now,
                        // since the template body is scanned in another mode.
                        if previous.can_end_expression() {
                            increment_source_operators(&frames, &mut current_operators, index)?;
                        }
                        mode = ScanMode::Template;
                        escaped = false;
                    }
                    (b'(', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Paren,
                        previous.can_end_expression(),
                        index,
                    )?,
                    (b')', _) => leave_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Paren,
                        index,
                    )?,
                    (b'[', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Bracket,
                        previous.can_end_expression(),
                        index,
                    )?,
                    (b']', _) => leave_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Bracket,
                        index,
                    )?,
                    (b'{', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        if previous.opens_expression_brace() {
                            SourceDelimiter::Brace
                        } else {
                            SourceDelimiter::StatementBrace
                        },
                        false,
                        index,
                    )?,
                    (b'}', _) => {
                        let closed =
                            frames
                                .last()
                                .map(|frame| frame.delimiter)
                                .filter(|delimiter| {
                                    matches!(
                                        delimiter,
                                        SourceDelimiter::Brace
                                            | SourceDelimiter::StatementBrace
                                            | SourceDelimiter::TemplateExpression
                                    )
                                });
                        if let Some(delimiter) = closed {
                            leave_source_delimiter(
                                &mut frames,
                                &mut current_operators,
                                delimiter,
                                index,
                            )?;
                        }
                        match closed {
                            // A statement block ends every statement form that
                            // introduced it, so the next statement starts over.
                            Some(SourceDelimiter::StatementBrace) => {
                                current_operators = 0;
                                open_statement_forms = 0;
                                open_conditionals = 0;
                            }
                            Some(SourceDelimiter::TemplateExpression) => mode = ScanMode::Template,
                            _ => {}
                        }
                    }
                    (b';' | b',', _) => {
                        current_operators = 0;
                        open_conditionals = 0;
                        // A `;` inside a delimiter is a `for` header separator
                        // and a `,` is an element separator; neither ends the
                        // statement that opened the delimiter. Clearing the
                        // open statement forms there would let a newline end a
                        // `for (;;)` that has not reached its body yet.
                        if frames.last().is_none_or(|frame: &SourceNestingFrame| {
                            frame.delimiter == SourceDelimiter::StatementBrace
                        }) {
                            open_statement_forms = 0;
                        }
                    }
                    (b':', _) => {
                        if open_conditionals > 0 {
                            // The second arm of a conditional, already charged
                            // by its `?`.
                            open_conditionals -= 1;
                        } else if previous.can_end_expression()
                            && frames.last().is_none_or(|frame: &SourceNestingFrame| {
                                frame.delimiter == SourceDelimiter::StatementBrace
                            })
                        {
                            // `LabelledStatement := Identifier ':' Statement`
                            // recurses without bound and uses no delimiter, so
                            // the label itself has to carry the charge. A type
                            // annotation in the same position charges one unit
                            // too, which its statement boundary releases.
                            increment_source_operators(&frames, &mut current_operators, index)?;
                        }
                    }
                    _ if is_identifier_start(byte) => {
                        let start = index;
                        while bytes
                            .get(index + 1)
                            .is_some_and(|byte| is_identifier_continue(*byte))
                        {
                            index += 1;
                        }
                        let word = &source[start..=index];
                        if is_recursive_operator_word(word) {
                            increment_source_operators(&frames, &mut current_operators, start)?;
                        }
                        if is_statement_form_word(word) {
                            open_statement_forms += 1;
                        }
                        scanned = Some(PreviousToken::word(word));
                    }
                    _ if is_recursive_operator_start(byte) => {
                        increment_source_operators(&frames, &mut current_operators, index)?;
                        let extra = recursive_operator_extra_bytes(bytes, index);
                        if byte == b'?' && extra == 0 {
                            open_conditionals += 1;
                        }
                        index += extra;
                    }
                    (b'\n' | b'\r', _) => {
                        // Automatic semicolon insertion ends the statement here
                        // unless the next token continues the expression, and
                        // only where a statement can actually end.
                        if previous.can_end_statement()
                            && open_statement_forms == 0
                            && frames.last().is_none_or(|frame: &SourceNestingFrame| {
                                frame.delimiter == SourceDelimiter::StatementBrace
                            })
                        {
                            pending_statement_end = true;
                        }
                        scanned = None;
                    }
                    _ if byte.is_ascii_whitespace() => scanned = None,
                    _ => {}
                }
                if let Some(token) = scanned {
                    previous = token;
                }
            }
            ScanMode::SingleQuoted => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::Byte(b'0');
                }
            }
            ScanMode::DoubleQuoted => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::Byte(b'0');
                }
            }
            ScanMode::Template => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'`' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::Byte(b'0');
                } else if byte == b'$' && next == Some(b'{') {
                    index += 1;
                    // A template lowers to a left-nested concatenation chain, so
                    // each hole deepens the tree the same way a `+` term does
                    // and has to keep drawing on the budget after it closes.
                    increment_source_operators(&frames, &mut current_operators, index)?;
                    enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::TemplateExpression,
                        false,
                        index,
                    )?;
                    mode = ScanMode::Code;
                    // A template hole is expression position, like `(`.
                    previous = PreviousToken::Byte(b'(');
                }
            }
            ScanMode::LineComment => {
                if matches!(byte, b'\n' | b'\r') {
                    mode = ScanMode::Code;
                    // This newline never reaches the code scanner, so apply the
                    // automatic-semicolon-insertion rule here instead.
                    if previous.can_end_statement()
                        && open_statement_forms == 0
                        && frames.last().is_none_or(|frame: &SourceNestingFrame| {
                            frame.delimiter == SourceDelimiter::StatementBrace
                        })
                    {
                        pending_statement_end = true;
                    }
                }
            }
            ScanMode::BlockComment => {
                if byte == b'*' && next == Some(b'/') {
                    mode = ScanMode::Code;
                    index += 1;
                }
            }
        }
        index += 1;
    }
    Ok(())
}

fn enter_source_delimiter(
    frames: &mut Vec<SourceNestingFrame>,
    current_operators: &mut usize,
    delimiter: SourceDelimiter,
    postfix: bool,
    index: usize,
) -> Result<(), Diagnostic> {
    let next_depth = source_nesting_depth(frames, *current_operators) + 1;
    if next_depth > MAX_SOURCE_NESTING_DEPTH {
        return Err(source_nesting_diagnostic(Some(SourceSpan {
            start: index,
            end: index + 1,
        })));
    }
    frames.push(SourceNestingFrame {
        delimiter,
        outer_operators: std::mem::take(current_operators),
        postfix,
    });
    Ok(())
}

fn leave_source_delimiter(
    frames: &mut Vec<SourceNestingFrame>,
    current_operators: &mut usize,
    delimiter: SourceDelimiter,
    index: usize,
) -> Result<(), Diagnostic> {
    if let Some(frame) = frames.pop_if(|frame| frame.delimiter == delimiter) {
        *current_operators = frame.outer_operators;
        if frame.postfix {
            increment_source_operators(frames, current_operators, index)?;
        }
    }
    Ok(())
}

fn increment_source_operators(
    frames: &[SourceNestingFrame],
    current_operators: &mut usize,
    index: usize,
) -> Result<(), Diagnostic> {
    *current_operators += 1;
    if source_nesting_depth(frames, *current_operators) > MAX_SOURCE_NESTING_DEPTH {
        return Err(source_nesting_diagnostic(Some(SourceSpan {
            start: index,
            end: index + 1,
        })));
    }
    Ok(())
}

fn source_nesting_depth(frames: &[SourceNestingFrame], current_operators: usize) -> usize {
    frames.len()
        + current_operators
        + frames
            .iter()
            .map(|frame| frame.outer_operators)
            .sum::<usize>()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_recursive_operator_word(word: &str) -> bool {
    matches!(
        word,
        // Value-position prefix and infix keyword operators.
        "await"
            | "delete"
            | "do"
            | "for"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "typeof"
            | "void"
            | "while"
            | "with"
            | "yield"
            // Cast operators: `Expression as Type` and `Expression satisfies
            // Type` are left-recursive in the grammar SWC parses.
            | "as"
            | "satisfies"
            // Type-position prefix operators. The dialect erases types, but SWC
            // parses them first and recurses through every one.
            | "asserts"
            | "infer"
            | "is"
            | "keyof"
            | "readonly"
            | "unique"
    )
}

/// Statement keywords that introduce a controlled statement, so the statement
/// is not complete until that inner statement is.
fn is_statement_form_word(word: &str) -> bool {
    matches!(word, "if" | "while" | "for" | "do" | "with" | "else")
}

/// Words after which a `{` can only open an object literal, never a block.
fn is_expression_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "await"
            | "case"
            | "delete"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "typeof"
            | "void"
            | "yield"
    )
}

fn is_recursive_operator_start(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'~'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'|'
            | b'^'
            | b'&'
            | b'?'
            | b'='
            | b'.'
    )
}

fn recursive_operator_extra_bytes(bytes: &[u8], index: usize) -> usize {
    const COMPOUND_OPERATORS: &[&[u8]] = &[
        b">>>=", b"===", b"!==", b">>>", b"**=", b"<<=", b">>=", b"||=", b"&&=", b"??=", b"...",
        b"=>", b"++", b"--", b"+=", b"-=", b"*=", b"/=", b"%=", b"|=", b"^=", b"&=", b"==", b"!=",
        b"<=", b">=", b"<<", b">>", b"**", b"||", b"&&", b"??", b"?.",
    ];
    COMPOUND_OPERATORS
        .iter()
        .find(|operator| bytes[index..].starts_with(operator))
        .map_or(0, |operator| operator.len() - 1)
}

pub(super) fn source_nesting_diagnostic(span: Option<SourceSpan>) -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::SourceNestingLimit,
        format!(
            "TypeScript source nesting exceeds the {MAX_SOURCE_NESTING_DEPTH}-level limit; flatten the source"
        ),
        span,
    )
}
