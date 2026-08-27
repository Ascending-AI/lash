//! The source-nesting preflight.

use super::{Diagnostic, DiagnosticCode, MAX_SOURCE_NESTING_DEPTH, SourceSpan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanMode {
    Code,
    SingleQuoted,
    DoubleQuoted,
    Template,
    RegExp,
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

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
enum SourceConstruct {
    Delimiter,
    Operator,
}

/// The source constructs that consume the shared nesting budget.
///
/// Keeping the entries separate makes the limit policy explicit without
/// splitting the cumulative counter: delimiters and operators must continue to
/// draw on the same budget.
const SOURCE_CONSTRUCT_LIMITS: [usize; 2] = [
    MAX_SOURCE_NESTING_DEPTH, // SourceConstruct::Delimiter
    MAX_SOURCE_NESTING_DEPTH, // SourceConstruct::Operator
];

#[derive(Default)]
struct SourceNestingState {
    frames: Vec<SourceNestingFrame>,
    current_operators: usize,
    open_statement_forms: usize,
    open_conditionals: usize,
}

impl SourceNestingState {
    fn enter(
        &mut self,
        delimiter: SourceDelimiter,
        postfix: bool,
        index: usize,
    ) -> Result<(), SourceSpan> {
        self.ensure_within_limit(SourceConstruct::Delimiter, self.depth() + 1, index)?;
        self.frames.push(SourceNestingFrame {
            delimiter,
            outer_operators: std::mem::take(&mut self.current_operators),
            postfix,
        });
        Ok(())
    }

    fn leave(&mut self, delimiter: SourceDelimiter, index: usize) -> Result<(), SourceSpan> {
        if let Some(frame) = self.frames.pop_if(|frame| frame.delimiter == delimiter) {
            self.current_operators = frame.outer_operators;
            if frame.postfix {
                self.increment_operator(index)?;
            }
        }
        Ok(())
    }

    fn increment_operator(&mut self, index: usize) -> Result<(), SourceSpan> {
        self.current_operators += 1;
        self.ensure_within_limit(SourceConstruct::Operator, self.depth(), index)
    }

    fn ensure_within_limit(
        &self,
        construct: SourceConstruct,
        depth: usize,
        index: usize,
    ) -> Result<(), SourceSpan> {
        if depth > SOURCE_CONSTRUCT_LIMITS[construct as usize] {
            return Err(SourceSpan {
                start: index,
                end: index + 1,
            });
        }
        Ok(())
    }

    fn depth(&self) -> usize {
        self.frames.len()
            + self.current_operators
            + self
                .frames
                .iter()
                .map(|frame| frame.outer_operators)
                .sum::<usize>()
    }

    fn current_delimiter(&self) -> Option<SourceDelimiter> {
        self.frames.last().map(|frame| frame.delimiter)
    }

    fn at_statement_level(&self) -> bool {
        self.frames
            .last()
            .is_none_or(|frame| frame.delimiter == SourceDelimiter::StatementBrace)
    }

    fn reset_statement(&mut self) {
        self.current_operators = 0;
        self.open_statement_forms = 0;
        self.open_conditionals = 0;
    }
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

    /// Whether this token can be a label name, which is a narrower question
    /// than either of the two above and must be answered on its own.
    ///
    /// `LabelledStatement := Identifier ':' Statement` accepts any identifier,
    /// and a contextual keyword — `type`, `of`, `let`, `keyof`, `as` — is an
    /// identifier. Deriving this from the automatic-semicolon-insertion
    /// exclusion list, which is about which *reserved words* can end a
    /// statement, made one predicate answer two different questions and left
    /// `type: type: type: …` charging nothing. Every word can name a label; so
    /// can a token the scanner classified as a bare identifier byte.
    fn can_name_a_label(self) -> bool {
        match self {
            Self::Word { .. } => true,
            Self::Byte(byte) => byte.is_ascii_alphanumeric() || byte >= 0x80,
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
// **This scan is no longer what keeps the host alive.** It was, for five
// rounds, and each round it was correct about the axis it modelled while the
// next abort sat one definition to the side: a grammar family nobody had
// enumerated, an identifier tokenised differently from SWC, a contextual
// keyword classified by the wrong predicate. That is what a second
// implementation of somebody else's lexer costs. The guarantee now rests on
// arithmetic instead — the source is bounded at `MAX_SOURCE_BYTES` and the
// parse runs on a stack reserved in proportion to it, with margin over the
// worst frame cost ever measured; see `parse_stack_size` in this module's
// parent for the numbers, and `tests/no_abort_guarantee.rs`, which runs every
// shape that ever aborted with this scan switched off.
//
// What the scan is for now is the diagnostic. `TS_SOURCE_NESTING_LIMIT` with
// source-level wording is a far better answer to a deeply nested cell than a
// parser-depth error or a successful parse of something unreadable, and a cheap
// pre-parse rejection keeps a pathological cell from costing a full parse. So
// the argument below still matters and the guards still enforce it — a missing
// charge is now a quality regression rather than a dead process.
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
// There is a second thing this scan has to get right, and it is not a grammar
// question at all: **its lexer must agree with SWC's about where each token
// ends.** Charging the right production is not enough if the token the charge
// is gated on was cut in half. Labels are the sharp case — the `:` charge fires
// only when the previous token was an identifier, and nothing else in a label
// carries a charge — so an identifier the scanner ends early silently disarms
// it. That is why identifier scanning treats every byte at or above `0x80` as
// an identifier character, walks `\uXXXX` and `\u{…}` identifier escapes, and
// stops at U+2028/U+2029.
//
// Agreement is the property; Unicode-class exactness is not. The
// classification deliberately over-approximates `ID_Start` / `ID_Continue`,
// which is the safe direction: including a byte SWC would have tokenised
// separately can only merge two tokens, and no charged token — every operator
// keyword and every punctuation operator is ASCII — can be swallowed by such a
// merge, so no charged shape becomes uncharged. Excluding a byte SWC includes
// is what fails, and it fails silently. `tests/depth_guard.rs` carries this as
// its own axis, and the fuzzer in `tests/grammar_coverage.rs` pairs non-ASCII
// atoms with charge-bearing tails for the same reason.
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
    SourceNestingVisitor::new(source)
        .scan()
        .map_err(|error| match error {
            SourceScanError::NestingLimit(span) => source_nesting_diagnostic(Some(span)),
            SourceScanError::Diagnostic(diagnostic) => diagnostic,
        })
}

enum SourceScanError {
    NestingLimit(SourceSpan),
    Diagnostic(Diagnostic),
}

struct SourceNestingVisitor<'source> {
    source: &'source str,
    bytes: &'source [u8],
    mode: ScanMode,
    escaped: bool,
    regexp_class: bool,
    regexp_pattern_start: usize,
    nesting: SourceNestingState,
    previous: PreviousToken,
    /// A newline that could end a statement; the following token decides
    /// whether it actually did.
    pending_statement_end: bool,
    index: usize,
}

impl<'source> SourceNestingVisitor<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            mode: ScanMode::Code,
            escaped: false,
            regexp_class: false,
            regexp_pattern_start: 0,
            nesting: SourceNestingState::default(),
            previous: PreviousToken::None,
            pending_statement_end: false,
            index: 0,
        }
    }

    fn scan(mut self) -> Result<(), SourceScanError> {
        while self.index < self.bytes.len() {
            self.visit_current()?;
            self.index += 1;
        }
        Ok(())
    }

    fn visit_current(&mut self) -> Result<(), SourceScanError> {
        let byte = self.bytes[self.index];
        let next = self.bytes.get(self.index + 1).copied();
        match self.mode {
            ScanMode::Code => self.visit_code(byte, next),
            ScanMode::SingleQuoted => {
                self.visit_quoted(byte, b'\'');
                Ok(())
            }
            ScanMode::DoubleQuoted => {
                self.visit_quoted(byte, b'"');
                Ok(())
            }
            ScanMode::Template => self.visit_template(byte, next),
            ScanMode::RegExp => self.visit_regexp(byte),
            ScanMode::LineComment => {
                self.visit_line_comment(byte);
                Ok(())
            }
            ScanMode::BlockComment => {
                self.visit_block_comment(byte, next);
                Ok(())
            }
        }
    }

    fn visit_code(&mut self, byte: u8, next: Option<u8>) -> Result<(), SourceScanError> {
        self.resolve_pending_statement_end(byte, next);

        // `None` leaves the previous significant token in place, so whitespace
        // and comments never change how a `{` is classified.
        let mut scanned = Some(PreviousToken::Byte(byte));
        match (byte, next) {
            (b'/', Some(b'/')) => {
                self.mode = ScanMode::LineComment;
                self.index += 1;
                scanned = None;
            }
            (b'/', Some(b'*')) => {
                self.mode = ScanMode::BlockComment;
                self.index += 1;
                scanned = None;
            }
            (b'/', _) if !self.previous.can_end_expression() => self.start_regexp(),
            (b'\'', _) => self.start_quoted(ScanMode::SingleQuoted),
            (b'"', _) => self.start_quoted(ScanMode::DoubleQuoted),
            (b'`', _) => self.start_template()?,
            (b'(', _) => {
                self.enter_delimiter(SourceDelimiter::Paren, self.previous.can_end_expression())?
            }
            (b')', _) => self.leave_delimiter(SourceDelimiter::Paren)?,
            (b'[', _) => {
                self.enter_delimiter(SourceDelimiter::Bracket, self.previous.can_end_expression())?
            }
            (b']', _) => self.leave_delimiter(SourceDelimiter::Bracket)?,
            (b'{', _) => self.enter_delimiter(
                if self.previous.opens_expression_brace() {
                    SourceDelimiter::Brace
                } else {
                    SourceDelimiter::StatementBrace
                },
                false,
            )?,
            (b'}', _) => self.close_brace()?,
            (b';' | b',', _) => self.visit_separator(),
            (b':', _) => self.visit_colon()?,
            _ if unicode_line_terminator_length(self.bytes, self.index).is_some() => {
                self.visit_unicode_line_terminator();
                scanned = None;
            }
            _ if is_identifier_start(byte) => {
                scanned = Some(self.visit_identifier()?);
            }
            _ if is_recursive_operator_start(byte) => self.visit_operator(byte)?,
            (b'\n' | b'\r', _) => {
                self.mark_pending_statement_end();
                scanned = None;
            }
            _ if byte.is_ascii_whitespace() => scanned = None,
            _ => {}
        }
        if let Some(token) = scanned {
            self.previous = token;
        }
        Ok(())
    }

    fn resolve_pending_statement_end(&mut self, byte: u8, next: Option<u8>) {
        if !self.pending_statement_end || byte.is_ascii_whitespace() {
            return;
        }
        let word = if is_identifier_start(byte)
            && unicode_line_terminator_length(self.bytes, self.index).is_none()
        {
            Some(&self.source[self.index..identifier_end(self.bytes, self.index)])
        } else {
            None
        };
        let comment = byte == b'/' && matches!(next, Some(b'/') | Some(b'*'));
        if !comment {
            self.pending_statement_end = false;
            if !continues_previous_statement(byte, word) {
                self.nesting.reset_statement();
            }
        }
    }

    fn start_regexp(&mut self) {
        // SWC lexes a RegExp literal as one token. Its grouping punctuation
        // therefore does not consume the TypeScript source-nesting budget; the
        // RegExp validator applies its separate group cap.
        self.mode = ScanMode::RegExp;
        self.escaped = false;
        self.regexp_class = false;
        self.regexp_pattern_start = self.index + 1;
    }

    fn start_quoted(&mut self, mode: ScanMode) {
        self.mode = mode;
        self.escaped = false;
    }

    fn start_template(&mut self) -> Result<(), SourceScanError> {
        // A tagged template is a postfix tail; charge it now, since the
        // template body is scanned in another mode.
        if self.previous.can_end_expression() {
            self.increment_operator()?;
        }
        self.mode = ScanMode::Template;
        self.escaped = false;
        Ok(())
    }

    fn enter_delimiter(
        &mut self,
        delimiter: SourceDelimiter,
        postfix: bool,
    ) -> Result<(), SourceScanError> {
        self.nesting
            .enter(delimiter, postfix, self.index)
            .map_err(SourceScanError::NestingLimit)
    }

    fn leave_delimiter(&mut self, delimiter: SourceDelimiter) -> Result<(), SourceScanError> {
        self.nesting
            .leave(delimiter, self.index)
            .map_err(SourceScanError::NestingLimit)
    }

    fn increment_operator(&mut self) -> Result<(), SourceScanError> {
        self.nesting
            .increment_operator(self.index)
            .map_err(SourceScanError::NestingLimit)
    }

    fn close_brace(&mut self) -> Result<(), SourceScanError> {
        let closed = self.nesting.current_delimiter().filter(|delimiter| {
            matches!(
                delimiter,
                SourceDelimiter::Brace
                    | SourceDelimiter::StatementBrace
                    | SourceDelimiter::TemplateExpression
            )
        });
        if let Some(delimiter) = closed {
            self.leave_delimiter(delimiter)?;
        }
        match closed {
            // A statement block ends every statement form that introduced it,
            // so the next statement starts over.
            Some(SourceDelimiter::StatementBrace) => self.nesting.reset_statement(),
            Some(SourceDelimiter::TemplateExpression) => self.mode = ScanMode::Template,
            _ => {}
        }
        Ok(())
    }

    fn visit_separator(&mut self) {
        self.nesting.current_operators = 0;
        self.nesting.open_conditionals = 0;
        // A `;` inside a delimiter is a `for` header separator and a `,` is an
        // element separator; neither ends the statement that opened the
        // delimiter. Clearing the open statement forms there would let a
        // newline end a `for (;;)` that has not reached its body yet.
        if self.nesting.at_statement_level() {
            self.nesting.open_statement_forms = 0;
        }
    }

    fn visit_colon(&mut self) -> Result<(), SourceScanError> {
        if self.nesting.open_conditionals > 0 {
            // The second arm of a conditional, already charged by its `?`.
            self.nesting.open_conditionals -= 1;
        } else if self.previous.can_name_a_label() && self.nesting.at_statement_level() {
            // `LabelledStatement := Identifier ':' Statement` recurses without
            // bound and uses no delimiter, so the label itself carries the
            // charge. A type annotation in the same position charges one unit
            // too, which its statement boundary releases.
            self.increment_operator()?;
        }
        Ok(())
    }

    fn visit_unicode_line_terminator(&mut self) {
        // U+2028 / U+2029 end a line in ECMAScript, so SWC inserts a semicolon
        // after them and the budget has to release in the same places.
        self.index += unicode_line_terminator_length(self.bytes, self.index)
            .expect("a line terminator was just matched")
            - 1;
        self.mark_pending_statement_end();
    }

    fn visit_identifier(&mut self) -> Result<PreviousToken, SourceScanError> {
        let start = self.index;
        self.index = identifier_end(self.bytes, self.index) - 1;
        let word = &self.source[start..=self.index];
        if is_recursive_operator_word(word) {
            self.nesting
                .increment_operator(start)
                .map_err(SourceScanError::NestingLimit)?;
        }
        if is_statement_form_word(word) {
            self.nesting.open_statement_forms += 1;
        }
        Ok(PreviousToken::word(word))
    }

    fn visit_operator(&mut self, byte: u8) -> Result<(), SourceScanError> {
        self.increment_operator()?;
        let extra = recursive_operator_extra_bytes(self.bytes, self.index);
        if byte == b'?' && extra == 0 {
            self.nesting.open_conditionals += 1;
        }
        self.index += extra;
        Ok(())
    }

    fn mark_pending_statement_end(&mut self) {
        // Statement forms whose controlled statement has not been reached yet
        // suppress automatic semicolon insertion: `if (1)` is not a complete
        // statement however the line breaks.
        if self.previous.can_end_statement()
            && self.nesting.open_statement_forms == 0
            && self.nesting.at_statement_level()
        {
            self.pending_statement_end = true;
        }
    }

    fn visit_quoted(&mut self, byte: u8, quote: u8) {
        if self.escaped {
            self.escaped = false;
        } else if byte == b'\\' {
            self.escaped = true;
        } else if byte == quote {
            self.mode = ScanMode::Code;
            self.previous = PreviousToken::Byte(b'0');
        }
    }

    fn visit_template(&mut self, byte: u8, next: Option<u8>) -> Result<(), SourceScanError> {
        if self.escaped {
            self.escaped = false;
        } else if byte == b'\\' {
            self.escaped = true;
        } else if byte == b'`' {
            self.mode = ScanMode::Code;
            self.previous = PreviousToken::Byte(b'0');
        } else if byte == b'$' && next == Some(b'{') {
            self.index += 1;
            // A template lowers to a left-nested concatenation chain, so each
            // hole deepens the tree like a `+` term and keeps drawing on the
            // budget after it closes.
            self.increment_operator()?;
            self.enter_delimiter(SourceDelimiter::TemplateExpression, false)?;
            self.mode = ScanMode::Code;
            // A template hole is expression position, like `(`.
            self.previous = PreviousToken::Byte(b'(');
        }
        Ok(())
    }

    fn visit_regexp(&mut self, byte: u8) -> Result<(), SourceScanError> {
        if self.escaped {
            self.escaped = false;
        } else if byte == b'\\' {
            self.escaped = true;
        } else if byte == b'[' {
            self.regexp_class = true;
        } else if byte == b']' {
            self.regexp_class = false;
        } else if byte == b'/' && !self.regexp_class {
            crate::regex::validate_literal_shape(
                &self.source[self.regexp_pattern_start..self.index],
                Some(SourceSpan {
                    start: self.regexp_pattern_start,
                    end: self.index,
                }),
            )
            .map_err(SourceScanError::Diagnostic)?;
            self.mode = ScanMode::Code;
            self.previous = PreviousToken::Byte(b'0');
        }
        Ok(())
    }

    fn visit_line_comment(&mut self, byte: u8) {
        let unicode_terminator = unicode_line_terminator_length(self.bytes, self.index);
        if matches!(byte, b'\n' | b'\r') || unicode_terminator.is_some() {
            self.index += unicode_terminator.unwrap_or(1) - 1;
            self.mode = ScanMode::Code;
            // This newline never reaches the code scanner, so apply the
            // automatic-semicolon-insertion rule here instead.
            self.mark_pending_statement_end();
        }
    }

    fn visit_block_comment(&mut self, byte: u8, next: Option<u8>) {
        if byte == b'*' && next == Some(b'/') {
            self.mode = ScanMode::Code;
            self.index += 1;
        }
    }
}

/// ECMAScript identifiers are Unicode, and the scanner only has to agree with
/// SWC about **where a token ends**, not about which code points are legal.
/// Every byte at or above `0x80` is therefore treated as part of an identifier:
/// the classification is a deliberate over-approximation of `ID_Start` /
/// `ID_Continue`.
///
/// Over-approximating is the safe direction. Folding a byte into a word that
/// SWC would have made its own token can only *merge* tokens, which leaves a
/// charge unfired at most where the merged token is charged anyway (an operator
/// keyword cannot contain a non-ASCII byte, so no charged word is ever
/// swallowed). Under-approximating is what fails: splitting an identifier in
/// half turns `previous` into a bare continuation byte, and any charge gated on
/// "the previous token was an identifier" — the label charge has no other
/// token to fall back on — silently stops firing.
///
/// The two exceptions carved out below are the Unicode line terminators
/// U+2028/U+2029, which SWC ends a line on, and which therefore must not be
/// swallowed into a word.
fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') || byte >= 0x80
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

/// `\u00e9` and `\u{e9}` inside an identifier are that identifier's
/// characters, so the scanner has to walk past them the way SWC does.
fn identifier_escape_length(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'\\') || bytes.get(index + 1) != Some(&b'u') {
        return None;
    }
    if bytes.get(index + 2) == Some(&b'{') {
        let mut end = index + 3;
        while bytes.get(end).is_some_and(u8::is_ascii_hexdigit) {
            end += 1;
        }
        if end > index + 3 && bytes.get(end) == Some(&b'}') {
            return Some(end + 1 - index);
        }
        return None;
    }
    if (index + 6) <= bytes.len()
        && bytes[index + 2..index + 6]
            .iter()
            .all(u8::is_ascii_hexdigit)
    {
        return Some(6);
    }
    None
}

/// One past the last byte of the identifier starting at `index`, walking
/// identifier escapes and stopping at a Unicode line terminator.
fn identifier_end(bytes: &[u8], index: usize) -> usize {
    let mut end = index;
    loop {
        if let Some(length) = identifier_escape_length(bytes, end) {
            end += length;
            continue;
        }
        match bytes.get(end) {
            Some(byte)
                if is_identifier_continue(*byte)
                    && unicode_line_terminator_length(bytes, end).is_none() =>
            {
                end += 1;
            }
            _ => return end.max(index + 1),
        }
    }
}

/// The UTF-8 encoding of U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR,
/// both of which end a line in ECMAScript.
fn unicode_line_terminator_length(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) == Some(&0xE2)
        && bytes.get(index + 1) == Some(&0x80)
        && matches!(bytes.get(index + 2), Some(0xA8 | 0xA9))
    {
        Some(3)
    } else {
        None
    }
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
    Diagnostic::with_repair(
        DiagnosticCode::SourceNestingLimit,
        format!("TypeScript source nesting exceeds the {MAX_SOURCE_NESTING_DEPTH}-level limit"),
        "flatten the source: name intermediate values instead of nesting expressions",
        span,
    )
}
