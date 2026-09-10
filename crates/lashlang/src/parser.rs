use crate::ast::{
    AssignPathStep, AssignTarget, AstString, BinaryOp, Declaration, Expr, ExpressionSourceSpan,
    FunctionDecl, FunctionParam, LabelMetadata, ListComprehensionClause, ProcessDecl, ProcessParam,
    ProcessSignalDecl, ProcessStartExpr, Program, TypeDecl, TypeExpr, TypeField, UnaryOp,
};
use crate::lexer::{LexError, Span, Token, TokenKind, lex};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("expected {expected}, found {found}")]
    Expected {
        expected: &'static str,
        found: String,
        span: Span,
    },
    #[error("unexpected {found}")]
    Unexpected { found: String, span: Span },
    #[error("`{keyword}` can only be used inside a loop")]
    LoopControlOutsideLoop { keyword: &'static str, span: Span },
    #[error("`{keyword}` can only be used inside a `process` body")]
    SessionProcessAdminOutsideBlock { keyword: &'static str, span: Span },
    #[error("`{keyword}` can't be used inside a `process` body")]
    ForegroundControlInsideProcess { keyword: &'static str, span: Span },
    #[error("`finish` requires a value; use `finish null` to finish with null")]
    MissingFinishValue { span: Span },
    #[error("`submit` was removed; use `finish <value>`")]
    SubmitRemoved { span: Span },
    #[error(
        "declarative trigger syntax has been removed; construct a source value and call the trigger registry register operation"
    )]
    DeclarativeTriggerRemoved { span: Span },
    #[error("invalid @label annotation: {message}")]
    InvalidLabelAnnotation { message: String, span: Span },
    #[error(
        "@label can annotate statements or process declarations, but not other declarations or another @label"
    )]
    InvalidLabelTarget { span: Span },
    #[error("expression nesting too deep (limit {limit}); flatten the program")]
    NestingTooDeep { limit: usize, span: Span },
}

impl ParseError {
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Lex(_) => None,
            Self::Expected { span, .. }
            | Self::Unexpected { span, .. }
            | Self::LoopControlOutsideLoop { span, .. }
            | Self::SessionProcessAdminOutsideBlock { span, .. }
            | Self::ForegroundControlInsideProcess { span, .. }
            | Self::MissingFinishValue { span }
            | Self::SubmitRemoved { span }
            | Self::DeclarativeTriggerRemoved { span }
            | Self::InvalidLabelAnnotation { span, .. }
            | Self::InvalidLabelTarget { span }
            | Self::NestingTooDeep { span, .. } => Some(*span),
        }
    }

    pub fn offset(&self) -> usize {
        match self {
            Self::Lex(err) => err.offset(),
            Self::Expected { span, .. }
            | Self::Unexpected { span, .. }
            | Self::LoopControlOutsideLoop { span, .. }
            | Self::SessionProcessAdminOutsideBlock { span, .. }
            | Self::ForegroundControlInsideProcess { span, .. }
            | Self::MissingFinishValue { span }
            | Self::SubmitRemoved { span }
            | Self::DeclarativeTriggerRemoved { span }
            | Self::InvalidLabelAnnotation { span, .. }
            | Self::InvalidLabelTarget { span }
            | Self::NestingTooDeep { span, .. } => span.start,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Counts `parse` calls made on this thread.
    ///
    /// Whether a parse happened is otherwise observable only as cost, so a law
    /// about parse *avoidance* — a linked-program cache hit must not re-parse
    /// its source — would have to be pinned by an allocation or timing
    /// threshold. This counts the calls exactly instead. It is thread-local
    /// because the test binary runs its tests in parallel and a shared counter
    /// would see every other test's parses.
    pub(crate) static PARSE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The number of `parse` calls made on this thread so far.
#[cfg(test)]
pub(crate) fn parse_calls() -> usize {
    PARSE_CALLS.with(std::cell::Cell::get)
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    #[cfg(test)]
    PARSE_CALLS.with(|calls| calls.set(calls.get() + 1));
    let tokens = lex(source)?;
    Parser {
        tokens,
        index: 0,
        loop_depth: 0,
        process_depth: 0,
        nesting_depth: 0,
    }
    .parse_program()
}

/// Parse exactly one expression in workflow-editing context.
///
/// Process-only and loop-only expressions are accepted because a workflow
/// graph field may belong to a node nested inside either construct. Callers
/// remain responsible for validating that the parsed expression is compatible
/// with the graph node kind that owns it.
pub fn parse_expression(source: &str) -> Result<Expr, ParseError> {
    let tokens = lex(source)?;
    match parse_expression_with_context(tokens.clone(), true) {
        Ok(expression) => Ok(expression),
        Err(_) => parse_expression_with_context(tokens, false),
    }
}

fn parse_expression_with_context(
    tokens: Vec<Token>,
    inside_process: bool,
) -> Result<Expr, ParseError> {
    let mut parser = Parser {
        tokens,
        index: 0,
        loop_depth: 1,
        process_depth: usize::from(inside_process),
        nesting_depth: 0,
    };
    let expression = parser.parse_statement_expr()?.into_expr();
    if !parser.at_eof() {
        let token = parser.peek();
        return Err(ParseError::Expected {
            expected: "end of expression",
            found: render_kind(&token.kind),
            span: token.span,
        });
    }
    Ok(expression)
}

/// Parse exactly one type expression for workflow-editing fields.
pub fn parse_type_expression(source: &str) -> Result<TypeExpr, ParseError> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        loop_depth: 0,
        process_depth: 0,
        nesting_depth: 0,
    };
    let ty = parser.parse_type_expr()?;
    if !parser.at_eof() {
        let token = parser.peek();
        return Err(ParseError::Expected {
            expected: "end of type expression",
            found: render_kind(&token.kind),
            span: token.span,
        });
    }
    Ok(ty)
}

/// Maximum syntactic nesting depth (nested expressions *and* nested blocks).
/// Bounds recursive-descent stack growth so adversarial model-emitted source
/// (deeply nested brackets or `if`/`for` bodies) returns a `ParseError` instead
/// of overflowing the native stack and aborting the host.
///
/// The deepest chain is expression nesting: each level descends the full
/// precedence ladder (`parse_expr` -> ternary -> or -> and -> compare -> add ->
/// mul -> unary -> postfix -> primary -> grouping -> `parse_expr`), roughly a
/// dozen native frames carrying the large parsed-expression/span bundle.
/// Empirically ~40 levels parse comfortably on a 2 MiB thread stack, so the
/// parser itself is not the binding constraint. The downstream AST walkers
/// (link, compile, execute) are: a block-bodied level (`parse_block` ->
/// `parse_statement_expr` -> `parse_if`/`parse_for`/`parse_while` ->
/// `parse_block`) is cheap to *parse* but builds **two** AST levels, an
/// `Expr::If`/`While`/`For` wrapping an `Expr::Block`, and those walkers cost
/// per AST level. At the old limit of 40 the deepest accepted `if` chain built
/// an 81-level tree and aborted the process in `link`, which the cap existed to
/// prevent.
///
/// So the limit is set from the other end: it is whatever keeps the worst
/// parsed shape inside [`crate::ast::MAX_AST_NESTING_DEPTH`], which is itself
/// measured against the 2 MiB budget. Two AST levels per syntactic level plus
/// the constant a statement contributes puts the worst shape at `2 * 30 + 3`,
/// inside the 64-level AST cap. Real generated programs nest only a handful
/// deep, so this remains ample headroom, and `tests/nesting_cap.rs` pins the
/// relation for a whole family of parsed shapes rather than asserting it.
pub(crate) const MAX_NESTING_DEPTH: usize = 30;

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    loop_depth: usize,
    process_depth: usize,
    nesting_depth: usize,
}

#[derive(Clone)]
struct ParsedExpr {
    expr: Expr,
    span: Span,
    source_spans: Vec<ExpressionSourceSpan>,
}

impl ParsedExpr {
    fn leaf(expr: Expr, span: Span) -> Self {
        Self {
            expr,
            span,
            source_spans: vec![ExpressionSourceSpan {
                path: Vec::new(),
                span,
            }],
        }
    }

    fn node(expr: Expr, span: Span, children: impl IntoIterator<Item = (u32, ParsedExpr)>) -> Self {
        let mut source_spans = vec![ExpressionSourceSpan {
            path: Vec::new(),
            span,
        }];
        for (child_index, child) in children {
            source_spans.extend(child.source_spans.into_iter().map(|mut source_span| {
                source_span.path.insert(0, child_index);
                source_span
            }));
        }
        Self {
            expr,
            span,
            source_spans,
        }
    }

    fn into_expr(self) -> Expr {
        self.expr
    }
}

struct ParsedAssignTarget {
    target: AssignTarget,
    index_spans: Vec<ParsedExpr>,
}

struct ParsedListComprehensionClause {
    clause: ListComprehensionClause,
    expr_span: ParsedExpr,
}

fn push_root_expression(
    expressions: &mut Vec<Expr>,
    source_spans: &mut Vec<ExpressionSourceSpan>,
    parsed: ParsedExpr,
) {
    let root = expressions.len() as u32;
    let ParsedExpr {
        expr,
        source_spans: parsed_source_spans,
        ..
    } = parsed;
    source_spans.extend(parsed_source_spans.into_iter().map(|mut source_span| {
        source_span.path.insert(0, root);
        source_span
    }));
    expressions.push(expr);
}

mod expressions;
mod statements;
mod types_and_tokens;

fn static_signal_name_arg(expr: &Expr, call: &'static str) -> Result<AstString, ParseError> {
    if let Expr::String(name) = expr {
        return Ok(name.clone());
    }
    Err(ParseError::Unexpected {
        found: format!("non-literal signal name in `{call}`"),
        span: Span { start: 0, end: 0 },
    })
}

#[derive(Clone, Copy)]
pub(crate) enum IdentifierPosition {
    DeclarationLead,
    StatementLead,
    LabelTarget,
    PrimaryExpression,
    Identifier,
}

impl IdentifierPosition {
    const fn mask(self) -> u8 {
        match self {
            Self::DeclarationLead => 1 << 0,
            Self::StatementLead => 1 << 1,
            Self::LabelTarget => 1 << 2,
            Self::PrimaryExpression => 1 << 3,
            Self::Identifier => 1 << 4,
        }
    }
}

const ALL_IDENTIFIER_POSITIONS: u8 = IdentifierPosition::DeclarationLead.mask()
    | IdentifierPosition::StatementLead.mask()
    | IdentifierPosition::LabelTarget.mask()
    | IdentifierPosition::PrimaryExpression.mask()
    | IdentifierPosition::Identifier.mask();
const STATEMENT_FORMS: u8 = IdentifierPosition::DeclarationLead.mask()
    | IdentifierPosition::StatementLead.mask()
    | IdentifierPosition::LabelTarget.mask();
const PRIMARY_FORMS: u8 = STATEMENT_FORMS | IdentifierPosition::PrimaryExpression.mask();
const DECLARATION_FORMS: u8 =
    IdentifierPosition::DeclarationLead.mask() | IdentifierPosition::LabelTarget.mask();

/// Bare-name reservations at the identifier-emission positions distinguished
/// by the canonical printer.
///
/// Lexer hard keywords retain their all-position refusal. Contextual rows map
/// to the parser routine that consumes them, and declaration dispatch plus the
/// post-label target guard reserve the four declaration names.
const RESERVED_NAME_POSITIONS: &[(&str, u8)] = &[
    // Lexer hard keywords: the lexer never produces an identifier token.
    ("if", ALL_IDENTIFIER_POSITIONS),
    ("else", ALL_IDENTIFIER_POSITIONS),
    ("for", ALL_IDENTIFIER_POSITIONS),
    ("in", ALL_IDENTIFIER_POSITIONS),
    ("await", ALL_IDENTIFIER_POSITIONS),
    ("cancel", ALL_IDENTIFIER_POSITIONS),
    ("submit", ALL_IDENTIFIER_POSITIONS),
    ("print", ALL_IDENTIFIER_POSITIONS),
    ("call", ALL_IDENTIFIER_POSITIONS),
    ("and", ALL_IDENTIFIER_POSITIONS),
    ("or", ALL_IDENTIFIER_POSITIONS),
    ("not", ALL_IDENTIFIER_POSITIONS),
    ("true", ALL_IDENTIFIER_POSITIONS),
    ("false", ALL_IDENTIFIER_POSITIONS),
    ("null", ALL_IDENTIFIER_POSITIONS),
    // Contextual statement forms consumed by `parse_statement_expr`.
    ("let", STATEMENT_FORMS),
    ("yield", STATEMENT_FORMS),
    ("wake", STATEMENT_FORMS),
    ("fail", STATEMENT_FORMS),
    ("finish", STATEMENT_FORMS),
    ("break", STATEMENT_FORMS),
    ("continue", STATEMENT_FORMS),
    ("while", STATEMENT_FORMS),
    // Primary-expression special forms consumed by `parse_primary`.
    ("parallel", PRIMARY_FORMS),
    ("sleep", PRIMARY_FORMS),
    ("start", PRIMARY_FORMS),
    ("Type", PRIMARY_FORMS),
    ("wait_signal", PRIMARY_FORMS),
    ("signal_run", PRIMARY_FORMS),
    // Module declaration dispatch consumed by `parse_program`.
    ("type", DECLARATION_FORMS),
    ("process", DECLARATION_FORMS),
    ("fn", DECLARATION_FORMS),
    ("trigger", DECLARATION_FORMS),
];

pub(crate) fn is_parser_reserved_name(name: &str, position: IdentifierPosition) -> bool {
    RESERVED_NAME_POSITIONS
        .iter()
        .find_map(|(reserved, positions)| (*reserved == name).then_some(*positions))
        .is_some_and(|positions| positions & position.mask() != 0)
}

fn token_can_be_key(kind: &TokenKind) -> bool {
    matches!(kind, TokenKind::Ident(_) | TokenKind::String(_)) || keyword_key_name(kind).is_some()
}

fn keyword_key_name(kind: &TokenKind) -> Option<&'static str> {
    Some(match kind {
        TokenKind::If => "if",
        TokenKind::Else => "else",
        TokenKind::For => "for",
        TokenKind::In => "in",
        TokenKind::Await => "await",
        TokenKind::Cancel => "cancel",
        TokenKind::Submit => "submit",
        TokenKind::Print => "print",
        TokenKind::Call => "call",
        TokenKind::Ident(name) if matches!(name.as_str(), "yield" | "wake" | "finish" | "fail") => {
            return Some(match name.as_str() {
                "yield" => "yield",
                "wake" => "wake",
                "finish" => "finish",
                "fail" => "fail",
                _ => unreachable!(),
            });
        }
        TokenKind::And => "and",
        TokenKind::Or => "or",
        TokenKind::Not => "not",
        TokenKind::True => "true",
        TokenKind::False => "false",
        TokenKind::Null => "null",
        _ => return None,
    })
}

fn token_can_start_expr(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Null
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Number(_)
            | TokenKind::String(_)
            | TokenKind::Ident(_)
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
            | TokenKind::Await
            | TokenKind::Minus
            | TokenKind::Bang
            | TokenKind::Not
    )
}

fn render_kind(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(name) => format!("identifier `{name}`"),
        TokenKind::String(value) => format!("string {:?}", value),
        TokenKind::Number(value) => format!("number {value}"),
        TokenKind::LBrace => "`{`".to_string(),
        TokenKind::RBrace => "`}`".to_string(),
        TokenKind::LParen => "`(`".to_string(),
        TokenKind::RParen => "`)`".to_string(),
        TokenKind::LBracket => "`[`".to_string(),
        TokenKind::RBracket => "`]`".to_string(),
        TokenKind::Comma => "`,`".to_string(),
        TokenKind::Colon => "`:`".to_string(),
        TokenKind::At => "`@`".to_string(),
        TokenKind::Question => "`?`".to_string(),
        TokenKind::Dot => "`.`".to_string(),
        TokenKind::Bang => "`!`".to_string(),
        TokenKind::Equal => "`=`".to_string(),
        TokenKind::DoubleEqual => "`==`".to_string(),
        TokenKind::BangEqual => "`!=`".to_string(),
        TokenKind::AndAnd => "`&&`".to_string(),
        TokenKind::OrOr => "`||`".to_string(),
        TokenKind::Pipe => "`|`".to_string(),
        TokenKind::Less => "`<`".to_string(),
        TokenKind::LessEqual => "`<=`".to_string(),
        TokenKind::Greater => "`>`".to_string(),
        TokenKind::GreaterEqual => "`>=`".to_string(),
        TokenKind::Plus => "`+`".to_string(),
        TokenKind::Minus => "`-`".to_string(),
        TokenKind::Star => "`*`".to_string(),
        TokenKind::Slash => "`/`".to_string(),
        TokenKind::Percent => "`%`".to_string(),
        TokenKind::If => "`if`".to_string(),
        TokenKind::Else => "`else`".to_string(),
        TokenKind::For => "`for`".to_string(),
        TokenKind::In => "`in`".to_string(),
        TokenKind::Await => "`await`".to_string(),
        TokenKind::Cancel => "`cancel`".to_string(),
        TokenKind::Submit => "`submit`".to_string(),
        TokenKind::Print => "`print`".to_string(),
        TokenKind::Call => "`call`".to_string(),
        TokenKind::And => "`and`".to_string(),
        TokenKind::Or => "`or`".to_string(),
        TokenKind::Not => "`not`".to_string(),
        TokenKind::True => "`true`".to_string(),
        TokenKind::False => "`false`".to_string(),
        TokenKind::Null => "`null`".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}

#[cfg(test)]
mod tests;
