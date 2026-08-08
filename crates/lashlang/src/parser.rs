use crate::ast::{
    AssignPathStep, AssignTarget, AstString, BinaryOp, Declaration, Expr, ExpressionSourceSpan,
    LabelMetadata, ListComprehensionClause, ProcessDecl, ProcessParam, ProcessSignalDecl,
    ProcessStartExpr, Program, TypeDecl, TypeExpr, TypeField, UnaryOp,
};
use crate::lexer::{LexError, Span, Token, TokenKind, lex};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
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

pub fn parse(source: &str) -> Result<Program, ParseError> {
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
/// Empirically ~40 levels parse comfortably on a 2 MiB thread stack while
/// leaving headroom for debug-test frames, so the limit is kept under that
/// cliff. Block nesting (`parse_block` ->
/// `parse_statement_expr` -> `parse_if`/`parse_for`/`parse_while` -> `parse_block`) is a
/// shallower per-level chain and shares the same budget, so any mix of the two
/// stays bounded. Real generated programs nest only a handful deep, so this is
/// ample headroom; capping here also bounds every downstream AST walker
/// (validate, lower, compile, eval), since the tree can never be deeper than
/// the parser allowed.
const MAX_NESTING_DEPTH: usize = 40;

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

impl Parser {
    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let capacity = (self.tokens.len() / 20).max(1);
        let mut declarations = Vec::new();
        let mut declaration_spans = Vec::new();
        let mut expressions = Vec::with_capacity(capacity);
        let mut expression_spans = Vec::with_capacity(capacity);
        let mut expression_source_spans = Vec::new();
        while !self.at_eof() {
            if matches!(self.peek_kind(), TokenKind::At) {
                let start = self.peek().span.start;
                let label = self.parse_label_annotation()?;
                if self.peek_contextual("process") && !self.peek_assignment_target() {
                    declarations.push(Declaration::Process(self.parse_process_decl(Some(label))?));
                    declaration_spans.push(self.span_from(start));
                    continue;
                }
                if self.peek_contextual("type")
                    || self.peek_contextual("trigger")
                    || matches!(self.peek_kind(), TokenKind::At)
                {
                    return Err(ParseError::InvalidLabelTarget {
                        span: self.peek().span,
                    });
                }
                let inner = self.parse_statement_expr()?;
                let end = self
                    .tokens
                    .get(self.index.saturating_sub(1))
                    .map(|token| token.span.end)
                    .unwrap_or(start);
                let span = Span { start, end };
                let expr = ParsedExpr::node(
                    Expr::LabelAnnotated {
                        label,
                        expr: Box::new(inner.expr.clone()),
                    },
                    span,
                    [(0, inner)],
                );
                expression_spans.push(span);
                push_root_expression(&mut expressions, &mut expression_source_spans, expr);
                continue;
            }
            if self.peek_contextual("type") && !self.peek_assignment_target() {
                let start = self.peek().span.start;
                declarations.push(Declaration::Type(self.parse_type_decl()?));
                declaration_spans.push(self.span_from(start));
                continue;
            }
            if self.peek_contextual("process") && !self.peek_assignment_target() {
                let start = self.peek().span.start;
                declarations.push(Declaration::Process(self.parse_process_decl(None)?));
                declaration_spans.push(self.span_from(start));
                continue;
            }
            if self.peek_contextual("trigger") && !self.peek_assignment_target() {
                return Err(ParseError::DeclarativeTriggerRemoved {
                    span: self.peek().span,
                });
            }
            let start = self.peek().span.start;
            let expr = self.parse_statement_expr()?;
            let end = self
                .tokens
                .get(self.index.saturating_sub(1))
                .map(|token| token.span.end)
                .unwrap_or(start);
            let span = Span { start, end };
            expression_spans.push(span);
            push_root_expression(&mut expressions, &mut expression_source_spans, expr);
        }
        Ok(Program::module_with_spans(
            declarations,
            declaration_spans,
            expressions,
            expression_spans,
            expression_source_spans,
        ))
    }

    fn span_from(&self, start: usize) -> Span {
        let end = self
            .tokens
            .get(self.index.saturating_sub(1))
            .map(|token| token.span.end)
            .unwrap_or(start);
        Span { start, end }
    }

    fn parse_type_decl(&mut self) -> Result<TypeDecl, ParseError> {
        self.expect_contextual("type")?;
        let name = self.expect_ident()?;
        self.expect_exact(TokenKind::Equal, "`=`")?;
        let ty = if matches!(self.peek_kind(), TokenKind::LBrace) {
            self.parse_type_object_body()?
        } else {
            self.parse_type_expr()?
        };
        Ok(TypeDecl { name, ty })
    }

    fn parse_process_decl(
        &mut self,
        label: Option<LabelMetadata>,
    ) -> Result<ProcessDecl, ParseError> {
        self.expect_contextual("process")?;
        let name = self.expect_ident()?;
        self.expect_exact(TokenKind::LParen, "`(`")?;
        let mut params = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            let param_name = self.expect_ident()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let ty = self.parse_type_expr()?;
            params.push(ProcessParam {
                name: param_name,
                ty,
            });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect_exact(TokenKind::RParen, "`)`")?;
        let signals = if self.peek_contextual("signals") && !self.peek_assignment_target() {
            self.parse_process_signal_decls()?
        } else {
            Vec::new()
        };
        let return_ty = if matches!(self.peek_kind(), TokenKind::Minus)
            && self
                .tokens
                .get(self.index + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Greater))
        {
            self.bump();
            self.bump();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.process_depth += 1;
        let body = self.parse_block()?;
        self.process_depth -= 1;
        Ok(ProcessDecl {
            name,
            params,
            signals,
            return_ty,
            label,
            body: body.into_expr(),
        })
    }

    fn parse_process_signal_decls(&mut self) -> Result<Vec<ProcessSignalDecl>, ParseError> {
        self.expect_contextual("signals")?;
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        let mut signals = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let name = self.expect_ident()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let ty = self.parse_type_expr()?;
            signals.push(ProcessSignalDecl { name, ty });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect_exact(TokenKind::RBrace, "`}`")?;
        Ok(signals)
    }

    fn parse_statement_expr(&mut self) -> Result<ParsedExpr, ParseError> {
        match self.peek_kind() {
            TokenKind::If => self.parse_if(),
            TokenKind::For => self.parse_for(),
            TokenKind::Submit => Err(ParseError::SubmitRemoved {
                span: self.peek().span,
            }),
            TokenKind::Cancel => self.parse_cancel(),
            TokenKind::Print => self.parse_print(),
            TokenKind::Call => Err(ParseError::Unexpected {
                found: "`call`".to_string(),
                span: self.peek().span,
            }),
            TokenKind::Ident(name) if name == "let" && !self.peek_assignment_target() => {
                self.parse_let_assign()
            }
            TokenKind::Ident(name)
                if matches!(name.as_str(), "yield" | "wake" | "fail")
                    && !self.peek_assignment_target() =>
            {
                self.parse_processes()
            }
            TokenKind::Ident(name) if name == "finish" && !self.peek_assignment_target() => {
                self.parse_finish()
            }
            TokenKind::Ident(name) if name == "break" && !self.peek_assignment_target() => {
                self.parse_loop_control("break")
            }
            TokenKind::Ident(name) if name == "continue" && !self.peek_assignment_target() => {
                self.parse_loop_control("continue")
            }
            TokenKind::Ident(name) if name == "while" && !self.peek_assignment_target() => {
                self.parse_while()
            }
            TokenKind::Ident(_) if self.peek_assignment_target() => self.parse_assign(),
            _ => self.parse_expr(),
        }
    }

    fn parse_let_assign(&mut self) -> Result<ParsedExpr, ParseError> {
        self.bump();
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        let target = self.parse_assignment_target()?;
        self.expect_exact(TokenKind::Equal, "`=`")?;
        let expr = self.parse_expr()?;
        let value_child_index = target.index_spans.len() as u32;
        let span = Span {
            start,
            end: expr.span.end,
        };
        let mut children = target
            .index_spans
            .into_iter()
            .enumerate()
            .map(|(index, expr)| (index as u32, expr))
            .collect::<Vec<_>>();
        children.push((value_child_index, expr));
        let value_expr = children
            .last()
            .expect("assignment value child")
            .1
            .expr
            .clone();
        Ok(ParsedExpr::node(
            Expr::Assign {
                target: target.target,
                expr: Box::new(value_expr),
            },
            span,
            children,
        ))
    }

    fn parse_assignment_target(&mut self) -> Result<ParsedAssignTarget, ParseError> {
        let root = self.expect_ident()?;
        let mut steps = Vec::new();
        let mut index_spans = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.bump();
                    steps.push(AssignPathStep::Field(self.expect_key_name()?));
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect_exact(TokenKind::RBracket, "`]`")?;
                    steps.push(AssignPathStep::Index(index.expr.clone()));
                    index_spans.push(index);
                }
                _ => break,
            }
        }
        Ok(ParsedAssignTarget {
            target: AssignTarget { root, steps },
            index_spans,
        })
    }

    fn parse_if(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.bump().span.start;
        let condition = self.parse_expr()?;
        let then_block = self.parse_block()?;
        let else_block = if matches!(self.peek_kind(), TokenKind::Else) {
            self.bump();
            if matches!(self.peek_kind(), TokenKind::If) {
                self.parse_if()?
            } else {
                self.parse_block()?
            }
        } else {
            ParsedExpr::leaf(
                Expr::Block(Vec::new()),
                Span {
                    start: then_block.span.end,
                    end: then_block.span.end,
                },
            )
        };
        let span = Span {
            start,
            end: else_block.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::If {
                condition: Box::new(condition.expr.clone()),
                then_block: Box::new(then_block.expr.clone()),
                else_block: Box::new(else_block.expr.clone()),
            },
            span,
            [(0, condition), (1, then_block), (2, else_block)],
        ))
    }

    fn parse_for(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.bump().span.start;
        let binding = self.expect_ident()?;
        self.expect_exact(TokenKind::In, "`in`")?;
        let iterable = self.parse_expr()?;
        self.loop_depth += 1;
        let body = self.parse_block()?;
        self.loop_depth -= 1;
        let span = Span {
            start,
            end: body.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::For {
                binding,
                iterable: Box::new(iterable.expr.clone()),
                body: Box::new(body.expr.clone()),
            },
            span,
            [(0, iterable), (1, body)],
        ))
    }

    fn parse_while(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.bump().span.start;
        let condition = self.parse_expr()?;
        self.loop_depth += 1;
        let body = self.parse_block()?;
        self.loop_depth -= 1;
        let span = Span {
            start,
            end: body.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::While {
                condition: Box::new(condition.expr.clone()),
                body: Box::new(body.expr.clone()),
            },
            span,
            [(0, condition), (1, body)],
        ))
    }

    fn parse_loop_control(&mut self, keyword: &'static str) -> Result<ParsedExpr, ParseError> {
        let span = self.bump().span;
        if self.loop_depth == 0 {
            return Err(ParseError::LoopControlOutsideLoop { keyword, span });
        }
        let expr = match keyword {
            "break" => Expr::Break,
            "continue" => Expr::Continue,
            _ => unreachable!("unknown loop control keyword"),
        };
        Ok(ParsedExpr::leaf(expr, span))
    }

    fn parse_finish(&mut self) -> Result<ParsedExpr, ParseError> {
        let token = self.bump().clone();
        if matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            return Err(ParseError::MissingFinishValue { span: token.span });
        }
        let expr = self.parse_expr()?;
        let span = Span {
            start: token.span.start,
            end: expr.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::Finish(Box::new(expr.expr.clone())),
            span,
            [(0, expr)],
        ))
    }

    fn parse_print(&mut self) -> Result<ParsedExpr, ParseError> {
        let span = self.bump().span;
        if self.process_depth > 0 {
            return Err(ParseError::ForegroundControlInsideProcess {
                keyword: "print",
                span,
            });
        }
        let expr = self.parse_expr()?;
        let span = Span {
            start: span.start,
            end: expr.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::Print(Box::new(expr.expr.clone())),
            span,
            [(0, expr)],
        ))
    }

    fn parse_processes(&mut self) -> Result<ParsedExpr, ParseError> {
        let token = self.bump().clone();
        let TokenKind::Ident(keyword) = token.kind else {
            unreachable!("process admins are contextual identifiers");
        };
        let keyword_static = match keyword.as_str() {
            "yield" => "yield",
            "wake" => "wake",
            "fail" => "fail",
            _ => unreachable!("unknown process admin keyword"),
        };
        if self.process_depth == 0 {
            return Err(ParseError::SessionProcessAdminOutsideBlock {
                keyword: keyword_static,
                span: token.span,
            });
        }
        match keyword_static {
            "yield" => {
                let expr = self.parse_expr()?;
                let span = Span {
                    start: token.span.start,
                    end: expr.span.end,
                };
                Ok(ParsedExpr::node(
                    Expr::Yield(Box::new(expr.expr.clone())),
                    span,
                    [(0, expr)],
                ))
            }
            "wake" => {
                let expr = self.parse_expr()?;
                let span = Span {
                    start: token.span.start,
                    end: expr.span.end,
                };
                Ok(ParsedExpr::node(
                    Expr::Wake(Box::new(expr.expr.clone())),
                    span,
                    [(0, expr)],
                ))
            }
            "fail" => {
                let expr = self.parse_expr()?;
                let span = Span {
                    start: token.span.start,
                    end: expr.span.end,
                };
                Ok(ParsedExpr::node(
                    Expr::Fail(Box::new(expr.expr.clone())),
                    span,
                    [(0, expr)],
                ))
            }
            _ => unreachable!("unknown process admin keyword"),
        }
    }

    fn parse_cancel(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.bump().span.start;
        let expr = self.parse_expr()?;
        let span = Span {
            start,
            end: expr.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::Cancel(Box::new(expr.expr.clone())),
            span,
            [(0, expr)],
        ))
    }

    /// Account for entering one more level of syntactic nesting, rejecting
    /// input that would recurse deep enough to overflow the native stack.
    /// Pair every successful call with [`Parser::leave_nesting`].
    fn enter_nesting(&mut self) -> Result<(), ParseError> {
        if self.nesting_depth >= MAX_NESTING_DEPTH {
            return Err(ParseError::NestingTooDeep {
                limit: MAX_NESTING_DEPTH,
                span: self.peek().span,
            });
        }
        self.nesting_depth += 1;
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.nesting_depth -= 1;
    }

    fn parse_block(&mut self) -> Result<ParsedExpr, ParseError> {
        // Nested blocks (`if`/`for` bodies, bare braces) recurse through here
        // without passing through `parse_expr`, so they need their own guard.
        self.enter_nesting()?;
        let result = self.parse_block_inner();
        self.leave_nesting();
        result
    }

    fn parse_block_inner(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        let mut expressions = Vec::new();
        let mut children = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            let expr = if matches!(self.peek_kind(), TokenKind::At) {
                self.parse_annotated_statement()?
            } else {
                self.parse_statement_expr()?
            };
            let child_index = expressions.len() as u32;
            expressions.push(expr.expr.clone());
            children.push((child_index, expr));
        }
        self.expect_exact(TokenKind::RBrace, "`}`")?;
        Ok(ParsedExpr::node(
            Expr::Block(expressions),
            self.span_from(start),
            children,
        ))
    }

    fn parse_annotated_statement(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        let label = self.parse_label_annotation()?;
        if matches!(self.peek_kind(), TokenKind::At)
            || self.peek_contextual("type")
            || self.peek_contextual("process")
            || self.peek_contextual("trigger")
        {
            return Err(ParseError::InvalidLabelTarget {
                span: self.peek().span,
            });
        }
        let expr = self.parse_statement_expr()?;
        let span = Span {
            start,
            end: expr.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::LabelAnnotated {
                label,
                expr: Box::new(expr.expr.clone()),
            },
            span,
            [(0, expr)],
        ))
    }

    fn parse_label_annotation(&mut self) -> Result<LabelMetadata, ParseError> {
        let span = self.peek().span;
        self.expect_exact(TokenKind::At, "`@`")?;
        self.expect_contextual("label")?;
        self.expect_exact(TokenKind::LParen, "`(`")?;

        let mut title = None;
        let mut description = None;
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            let key_span = self.peek().span;
            let key = self.expect_ident()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let value = self.expect_string_literal()?;
            match key.as_str() {
                "title" => {
                    if title.replace(value).is_some() {
                        return Err(ParseError::InvalidLabelAnnotation {
                            message: "duplicate `title` field".to_string(),
                            span: key_span,
                        });
                    }
                }
                "description" => {
                    if description.replace(value).is_some() {
                        return Err(ParseError::InvalidLabelAnnotation {
                            message: "duplicate `description` field".to_string(),
                            span: key_span,
                        });
                    }
                }
                _ => {
                    return Err(ParseError::InvalidLabelAnnotation {
                        message: format!("unknown field `{key}`"),
                        span: key_span,
                    });
                }
            }
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect_exact(TokenKind::RParen, "`)`")?;
        let Some(title) = title else {
            return Err(ParseError::InvalidLabelAnnotation {
                message: "`title` is required".to_string(),
                span,
            });
        };
        Ok(LabelMetadata { title, description })
    }

    fn parse_expr(&mut self) -> Result<ParsedExpr, ParseError> {
        // Every nested expression (parenthesised, list/record element, index,
        // ternary, ...) funnels through here, so one depth guard at this
        // chokepoint bounds total recursive-descent stack growth.
        self.enter_nesting()?;
        let result = self.parse_tuple_expr();
        self.leave_nesting();
        result
    }

    fn parse_expr_no_tuple(&mut self) -> Result<ParsedExpr, ParseError> {
        self.enter_nesting()?;
        let result = self.parse_ternary();
        self.leave_nesting();
        result
    }

    fn parse_tuple_expr(&mut self) -> Result<ParsedExpr, ParseError> {
        let first = self.parse_ternary()?;
        if !matches!(self.peek_kind(), TokenKind::Comma) {
            return Ok(first);
        }

        let mut items = Vec::new();
        let mut children = Vec::new();
        children.push((0, first));
        items.push(children.last().expect("tuple item").1.expr.clone());
        let mut end = children.last().expect("tuple item").1.span.end;

        while matches!(self.peek_kind(), TokenKind::Comma) {
            end = self.bump().span.end;
            if matches!(
                self.peek_kind(),
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace | TokenKind::Eof
            ) {
                break;
            }
            let item = self.parse_ternary()?;
            end = item.span.end;
            children.push((items.len() as u32, item));
            items.push(children.last().expect("tuple item").1.expr.clone());
        }

        let span = Span {
            start: children.first().expect("tuple first").1.span.start,
            end,
        };
        Ok(ParsedExpr::node(Expr::Tuple(items), span, children))
    }

    fn parse_ternary(&mut self) -> Result<ParsedExpr, ParseError> {
        let condition = self.parse_or()?;
        if !matches!(self.peek_kind(), TokenKind::Question) {
            return Ok(condition);
        }
        self.bump();
        let then_expr = self.parse_expr_no_tuple()?;
        self.expect_exact(TokenKind::Colon, "`:`")?;
        let else_expr = self.parse_expr_no_tuple()?;
        let span = Span {
            start: condition.span.start,
            end: else_expr.span.end,
        };
        Ok(ParsedExpr::node(
            Expr::If {
                condition: Box::new(condition.expr.clone()),
                then_block: Box::new(then_expr.expr.clone()),
                else_block: Box::new(else_expr.expr.clone()),
            },
            span,
            [(0, condition), (1, then_expr), (2, else_expr)],
        ))
    }

    fn parse_or(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_and()?;
        while matches!(self.peek_kind(), TokenKind::Or | TokenKind::OrOr) {
            self.bump();
            let right = self.parse_and()?;
            let span = Span {
                start: expr.span.start,
                end: right.span.end,
            };
            expr = ParsedExpr::node(
                Expr::Binary {
                    left: Box::new(expr.expr.clone()),
                    op: BinaryOp::Or,
                    right: Box::new(right.expr.clone()),
                },
                span,
                [(0, expr), (1, right)],
            );
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_compare()?;
        while matches!(self.peek_kind(), TokenKind::And | TokenKind::AndAnd) {
            self.bump();
            let right = self.parse_compare()?;
            let span = Span {
                start: expr.span.start,
                end: right.span.end,
            };
            expr = ParsedExpr::node(
                Expr::Binary {
                    left: Box::new(expr.expr.clone()),
                    op: BinaryOp::And,
                    right: Box::new(right.expr.clone()),
                },
                span,
                [(0, expr), (1, right)],
            );
        }
        Ok(expr)
    }

    fn parse_compare(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_add()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::DoubleEqual => BinaryOp::Equal,
                TokenKind::BangEqual => BinaryOp::NotEqual,
                TokenKind::Less => BinaryOp::Less,
                TokenKind::LessEqual => BinaryOp::LessEqual,
                TokenKind::Greater => BinaryOp::Greater,
                TokenKind::GreaterEqual => BinaryOp::GreaterEqual,
                TokenKind::In => BinaryOp::In,
                _ => break,
            };
            self.bump();
            let right = self.parse_add()?;
            let span = Span {
                start: expr.span.start,
                end: right.span.end,
            };
            expr = ParsedExpr::node(
                Expr::Binary {
                    left: Box::new(expr.expr.clone()),
                    op,
                    right: Box::new(right.expr.clone()),
                },
                span,
                [(0, expr), (1, right)],
            );
        }
        Ok(expr)
    }

    fn parse_add(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_mul()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Subtract,
                _ => break,
            };
            self.bump();
            let right = self.parse_mul()?;
            let span = Span {
                start: expr.span.start,
                end: right.span.end,
            };
            expr = ParsedExpr::node(
                Expr::Binary {
                    left: Box::new(expr.expr.clone()),
                    op,
                    right: Box::new(right.expr.clone()),
                },
                span,
                [(0, expr), (1, right)],
            );
        }
        Ok(expr)
    }

    fn parse_mul(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinaryOp::Multiply,
                TokenKind::Slash => BinaryOp::Divide,
                TokenKind::Percent => BinaryOp::Modulo,
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            let span = Span {
                start: expr.span.start,
                end: right.span.end,
            };
            expr = ParsedExpr::node(
                Expr::Binary {
                    left: Box::new(expr.expr.clone()),
                    op,
                    right: Box::new(right.expr.clone()),
                },
                span,
                [(0, expr), (1, right)],
            );
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<ParsedExpr, ParseError> {
        match self.peek_kind() {
            TokenKind::Minus => {
                let start = self.bump().span.start;
                let expr = self.parse_unary()?;
                Ok(ParsedExpr::node(
                    Expr::Unary {
                        op: UnaryOp::Negate,
                        expr: Box::new(expr.expr.clone()),
                    },
                    Span {
                        start,
                        end: expr.span.end,
                    },
                    [(0, expr)],
                ))
            }
            TokenKind::Not => {
                let start = self.bump().span.start;
                let expr = self.parse_unary()?;
                Ok(ParsedExpr::node(
                    Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(expr.expr.clone()),
                    },
                    Span {
                        start,
                        end: expr.span.end,
                    },
                    [(0, expr)],
                ))
            }
            TokenKind::Bang => {
                let start = self.bump().span.start;
                let expr = self.parse_unary()?;
                Ok(ParsedExpr::node(
                    Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(expr.expr.clone()),
                    },
                    Span {
                        start,
                        end: expr.span.end,
                    },
                    [(0, expr)],
                ))
            }
            TokenKind::Await => {
                let start = self.bump().span.start;
                let expr = self.parse_unary()?;
                Ok(ParsedExpr::node(
                    Expr::Await(Box::new(expr.expr.clone())),
                    Span {
                        start,
                        end: expr.span.end,
                    },
                    [(0, expr)],
                ))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<ParsedExpr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.bump();
                    let field = self.expect_key_name()?;
                    if matches!(self.peek_kind(), TokenKind::LParen) {
                        let args = self.parse_call_arguments()?;
                        let span = self.span_from(expr.span.start);
                        let mut children = Vec::with_capacity(args.len() + 1);
                        children.push((0, expr));
                        let arg_exprs = args.iter().map(|arg| arg.expr.clone()).collect();
                        children.extend(
                            args.into_iter()
                                .enumerate()
                                .map(|(index, arg)| ((index + 1) as u32, arg)),
                        );
                        let receiver = children[0].1.expr.clone();
                        expr = ParsedExpr::node(
                            Expr::ReceiverCall {
                                receiver: Box::new(receiver),
                                operation: field,
                                args: arg_exprs,
                            },
                            span,
                            children,
                        );
                    } else {
                        let span = self.span_from(expr.span.start);
                        expr = ParsedExpr::node(
                            Expr::Field {
                                target: Box::new(expr.expr.clone()),
                                field,
                            },
                            span,
                            [(0, expr)],
                        );
                    }
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect_exact(TokenKind::RBracket, "`]`")?;
                    let span = self.span_from(expr.span.start);
                    expr = ParsedExpr::node(
                        Expr::Index {
                            target: Box::new(expr.expr.clone()),
                            index: Box::new(index.expr.clone()),
                        },
                        span,
                        [(0, expr), (1, index)],
                    );
                }
                TokenKind::Question if !self.question_starts_ternary() => {
                    self.bump();
                    let span = self.span_from(expr.span.start);
                    expr = ParsedExpr::node(
                        Expr::ResultUnwrap(Box::new(expr.expr.clone())),
                        span,
                        [(0, expr)],
                    );
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<ParsedExpr, ParseError> {
        match self.peek_kind() {
            TokenKind::Null => {
                let span = self.bump().span;
                Ok(ParsedExpr::leaf(Expr::Null, span))
            }
            TokenKind::True => {
                let span = self.bump().span;
                Ok(ParsedExpr::leaf(Expr::Bool(true), span))
            }
            TokenKind::False => {
                let span = self.bump().span;
                Ok(ParsedExpr::leaf(Expr::Bool(false), span))
            }
            TokenKind::Number(value) => {
                let value = *value;
                let span = self.bump().span;
                Ok(ParsedExpr::leaf(Expr::Number(value), span))
            }
            TokenKind::String(value) => {
                let value = value.clone();
                let span = self.bump().span;
                Ok(ParsedExpr::leaf(Expr::String(value), span))
            }
            TokenKind::Ident(name) => {
                let token_span = self.peek().span;
                if name == "parallel"
                    && self
                        .tokens
                        .get(self.index + 1)
                        .is_some_and(|token| matches!(token.kind, TokenKind::LBrace))
                {
                    return Err(ParseError::Unexpected {
                        found: "`parallel`".to_string(),
                        span: self.peek().span,
                    });
                }
                let name = name.clone();
                self.bump();
                if name == "sleep" {
                    return self.parse_sleep_expr(token_span.start);
                }
                if name == "start" && matches!(self.peek_kind(), TokenKind::Call) {
                    return Err(ParseError::Unexpected {
                        found: "`start call`".to_string(),
                        span: self.tokens[self.index.saturating_sub(1)].span,
                    });
                }
                if name == "start"
                    && (matches!(self.peek_kind(), TokenKind::Ident(_))
                        || matches!(self.peek_kind(), TokenKind::LBrace)
                        || self.paren_group_followed_by_lbrace())
                {
                    return self.parse_process_start_expr(token_span.start);
                }
                if name == "Type" && matches!(self.peek_kind(), TokenKind::LBrace) {
                    let ty = self.parse_type_object()?;
                    return Ok(ParsedExpr::leaf(
                        Expr::TypeLiteral(Box::new(ty)),
                        self.span_from(token_span.start),
                    ));
                }
                if matches!(self.peek_kind(), TokenKind::LParen) {
                    let args = self.parse_call_arguments()?;
                    if name == "wait_signal" {
                        if args.len() != 1 {
                            return Err(ParseError::Expected {
                                expected: "one signal name argument",
                                found: format!("{} arguments", args.len()),
                                span: self.tokens[self.index.saturating_sub(1)].span,
                            });
                        }
                        return Ok(ParsedExpr::leaf(
                            Expr::WaitSignal {
                                name: static_signal_name_arg(&args[0].expr, "wait_signal")?,
                            },
                            self.span_from(token_span.start),
                        ));
                    }
                    if name == "signal_run" {
                        if args.len() != 3 {
                            return Err(ParseError::Expected {
                                expected: "run handle, signal name, and payload arguments",
                                found: format!("{} arguments", args.len()),
                                span: self.tokens[self.index.saturating_sub(1)].span,
                            });
                        }
                        let run = args[0].expr.clone();
                        let payload = args[2].expr.clone();
                        return Ok(ParsedExpr::node(
                            Expr::SignalRun {
                                run: Box::new(run),
                                name: static_signal_name_arg(&args[1].expr, "signal_run")?,
                                payload: Box::new(payload),
                            },
                            self.span_from(token_span.start),
                            [(0, args[0].clone()), (1, args[2].clone())],
                        ));
                    }
                    let arg_exprs = args.iter().map(|arg| arg.expr.clone()).collect();
                    Ok(ParsedExpr::node(
                        Expr::BuiltinCall {
                            name,
                            args: arg_exprs,
                        },
                        self.span_from(token_span.start),
                        args.into_iter()
                            .enumerate()
                            .map(|(index, arg)| (index as u32, arg)),
                    ))
                } else {
                    Ok(ParsedExpr::leaf(Expr::Variable(name), token_span))
                }
            }
            TokenKind::LParen => {
                let start = self.bump().span.start;
                if matches!(self.peek_kind(), TokenKind::RParen) {
                    self.bump();
                    return Ok(ParsedExpr::node(
                        Expr::Tuple(Vec::new()),
                        self.span_from(start),
                        [],
                    ));
                }
                let expr = self.parse_expr()?;
                self.expect_exact(TokenKind::RParen, "`)`")?;
                Ok(expr)
            }
            TokenKind::LBracket => self.parse_list(),
            TokenKind::LBrace => self.parse_record(),
            TokenKind::Submit => Err(ParseError::SubmitRemoved {
                span: self.peek().span,
            }),
            TokenKind::Call => Err(ParseError::Unexpected {
                found: "`call`".to_string(),
                span: self.peek().span,
            }),
            _ => Err(self.unexpected()),
        }
    }

    fn parse_list(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        self.expect_exact(TokenKind::LBracket, "`[`")?;
        if matches!(self.peek_kind(), TokenKind::RBracket) {
            self.expect_exact(TokenKind::RBracket, "`]`")?;
            return Ok(ParsedExpr::node(
                Expr::List(Vec::new()),
                self.span_from(start),
                [],
            ));
        }

        let first = self.parse_expr_no_tuple()?;
        if matches!(self.peek_kind(), TokenKind::For) {
            let parsed_clauses = self.parse_list_comprehension_clauses()?;
            self.expect_exact(TokenKind::RBracket, "`]`")?;
            let clauses = parsed_clauses
                .iter()
                .map(|parsed| parsed.clause.clone())
                .collect::<Vec<_>>();
            let mut children = parsed_clauses
                .into_iter()
                .enumerate()
                .map(|(index, parsed)| (index as u32, parsed.expr_span))
                .collect::<Vec<_>>();
            children.push((children.len() as u32, first.clone()));
            return Ok(ParsedExpr::node(
                Expr::ListComprehension {
                    element: Box::new(first.expr.clone()),
                    clauses,
                },
                self.span_from(start),
                children,
            ));
        }

        let mut items = Vec::new();
        let mut children = Vec::new();
        children.push((0, first));
        items.push(children.last().expect("item").1.expr.clone());
        if matches!(self.peek_kind(), TokenKind::Comma) {
            self.bump();
        } else {
            self.expect_exact(TokenKind::RBracket, "`]`")?;
            return Ok(ParsedExpr::node(
                Expr::List(items),
                self.span_from(start),
                children,
            ));
        }
        while !matches!(self.peek_kind(), TokenKind::RBracket) {
            let item = self.parse_expr_no_tuple()?;
            children.push((items.len() as u32, item));
            items.push(children.last().expect("item").1.expr.clone());
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect_exact(TokenKind::RBracket, "`]`")?;
        Ok(ParsedExpr::node(
            Expr::List(items),
            self.span_from(start),
            children,
        ))
    }

    fn parse_list_comprehension_clauses(
        &mut self,
    ) -> Result<Vec<ParsedListComprehensionClause>, ParseError> {
        let mut clauses = Vec::new();
        while matches!(self.peek_kind(), TokenKind::For) {
            self.bump();
            let binding = self.expect_ident()?;
            self.expect_exact(TokenKind::In, "`in`")?;
            let iterable = self.parse_expr()?;
            clauses.push(ParsedListComprehensionClause {
                clause: ListComprehensionClause::For {
                    binding,
                    iterable: iterable.expr.clone(),
                },
                expr_span: iterable,
            });
            while matches!(self.peek_kind(), TokenKind::If) {
                self.bump();
                let condition = self.parse_expr()?;
                clauses.push(ParsedListComprehensionClause {
                    clause: ListComprehensionClause::If {
                        condition: condition.expr.clone(),
                    },
                    expr_span: condition,
                });
            }
        }
        Ok(clauses)
    }

    fn parse_record(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        let entries = self.parse_record_entries()?;
        self.expect_exact(TokenKind::RBrace, "`}`")?;
        let expr_entries = entries
            .iter()
            .map(|(key, value)| (key.clone(), value.expr.clone()))
            .collect();
        Ok(ParsedExpr::node(
            Expr::Record(expr_entries),
            self.span_from(start),
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (_, value))| (index as u32, value)),
        ))
    }

    fn parse_record_entries(&mut self) -> Result<Vec<(AstString, ParsedExpr)>, ParseError> {
        let mut entries = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace) {
            let key = self.expect_key_name()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let value = self.parse_expr_no_tuple()?;
            entries.push((key, value));
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        Ok(entries)
    }

    fn parse_call_arguments(&mut self) -> Result<Vec<ParsedExpr>, ParseError> {
        self.expect_exact(TokenKind::LParen, "`(`")?;
        let mut args = Vec::new();
        if !matches!(self.peek_kind(), TokenKind::RParen) {
            loop {
                args.push(self.parse_expr_no_tuple()?);
                if matches!(self.peek_kind(), TokenKind::Comma) {
                    self.bump();
                    if matches!(self.peek_kind(), TokenKind::RParen) {
                        break;
                    }
                    continue;
                }
                break;
            }
        }
        self.expect_exact(TokenKind::RParen, "`)`")?;
        Ok(args)
    }

    fn parse_named_arguments(&mut self) -> Result<Vec<(AstString, ParsedExpr)>, ParseError> {
        let mut entries = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            let key = self.expect_key_name()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let value = self.parse_expr_no_tuple()?;
            entries.push((key, value));
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        Ok(entries)
    }

    fn parse_process_start_expr(&mut self, start: usize) -> Result<ParsedExpr, ParseError> {
        if matches!(self.peek_kind(), TokenKind::LBrace) || self.paren_group_followed_by_lbrace() {
            return Err(ParseError::Unexpected {
                found: "inline `start` process body".to_string(),
                span: self.peek().span,
            });
        }
        let process = self.expect_ident()?;
        self.expect_exact(TokenKind::LParen, "`(`")?;
        let args = self.parse_named_arguments()?;
        self.expect_exact(TokenKind::RParen, "`)`")?;
        let expr_args = args
            .iter()
            .map(|(name, value)| (name.clone(), value.expr.clone()))
            .collect();
        Ok(ParsedExpr::node(
            Expr::StartProcess(ProcessStartExpr {
                process,
                args: expr_args,
            }),
            self.span_from(start),
            args.into_iter()
                .enumerate()
                .map(|(index, (_, value))| (index as u32, value)),
        ))
    }

    fn parse_sleep_expr(&mut self, start: usize) -> Result<ParsedExpr, ParseError> {
        if matches!(self.peek_kind(), TokenKind::For) {
            self.bump();
            let expr = self.parse_expr()?;
            return Ok(ParsedExpr::node(
                Expr::SleepFor(Box::new(expr.expr.clone())),
                Span {
                    start,
                    end: expr.span.end,
                },
                [(0, expr)],
            ));
        }
        if self.peek_contextual("until") {
            self.bump();
            let expr = self.parse_expr()?;
            return Ok(ParsedExpr::node(
                Expr::SleepUntil(Box::new(expr.expr.clone())),
                Span {
                    start,
                    end: expr.span.end,
                },
                [(0, expr)],
            ));
        }
        Err(ParseError::Expected {
            expected: "`for` or `until`",
            found: render_kind(self.peek_kind()),
            span: self.peek().span,
        })
    }

    fn parse_type_object(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        self.parse_type_object_body_after_lbrace()
    }

    fn parse_type_object_body(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        self.parse_type_object_body_after_lbrace()
    }

    fn parse_type_object_body_after_lbrace(&mut self) -> Result<TypeExpr, ParseError> {
        let mut fields = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace) {
            let name_token_span = self.peek().span;
            let name = self.expect_key_name()?;
            if !seen.insert(name.clone()) {
                return Err(ParseError::Expected {
                    expected: "unique field name",
                    found: format!("duplicate field `{name}`"),
                    span: name_token_span,
                });
            }
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let ty = self.parse_type_expr()?;
            let optional = if matches!(self.peek_kind(), TokenKind::Question) {
                self.bump();
                true
            } else {
                false
            };
            fields.push(TypeField { name, ty, optional });
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.bump();
                continue;
            }
            break;
        }
        self.expect_exact(TokenKind::RBrace, "`}`")?;
        Ok(TypeExpr::Object(fields))
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr, ParseError> {
        let first = self.parse_type_term()?;
        if !matches!(self.peek_kind(), TokenKind::Pipe) {
            return Ok(first);
        }
        // Union: `str | null`, `int | str | null`, etc. `|` has lower
        // precedence than any other type constructor — once we see it
        // at top level, keep parsing `| <term>` until the run ends.
        let mut variants = vec![first];
        while matches!(self.peek_kind(), TokenKind::Pipe) {
            self.bump();
            variants.push(self.parse_type_term()?);
        }
        Ok(TypeExpr::Union(variants))
    }

    fn parse_type_term(&mut self) -> Result<TypeExpr, ParseError> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Null => {
                self.bump();
                Ok(TypeExpr::Null)
            }
            TokenKind::String(value) => {
                self.bump();
                Ok(TypeExpr::Enum(vec![value]))
            }
            TokenKind::LBrace => self.parse_type_object_body(),
            TokenKind::Ident(_name) => {
                let name = self.parse_type_name()?;
                match name.as_str() {
                    "str" | "string" => Ok(TypeExpr::Str),
                    "int" | "integer" => Ok(TypeExpr::Int),
                    "float" | "number" => Ok(TypeExpr::Float),
                    "bool" | "boolean" => Ok(TypeExpr::Bool),
                    "dict" | "object" => Ok(TypeExpr::Dict),
                    "any" => Ok(TypeExpr::Any),
                    "enum" => {
                        self.expect_exact(TokenKind::LBracket, "`[`")?;
                        let mut values = Vec::new();
                        if !matches!(self.peek_kind(), TokenKind::RBracket) {
                            loop {
                                let value = self.expect_string_literal()?;
                                values.push(value);
                                if matches!(self.peek_kind(), TokenKind::Comma) {
                                    self.bump();
                                    continue;
                                }
                                break;
                            }
                        }
                        if values.is_empty() {
                            return Err(ParseError::Expected {
                                expected: "at least one enum string literal",
                                found: "empty enum".to_string(),
                                span: token.span,
                            });
                        }
                        self.expect_exact(TokenKind::RBracket, "`]`")?;
                        Ok(TypeExpr::Enum(values))
                    }
                    "list" => {
                        self.expect_exact(TokenKind::LBracket, "`[`")?;
                        let inner = self.parse_type_expr()?;
                        self.expect_exact(TokenKind::RBracket, "`]`")?;
                        Ok(TypeExpr::List(Box::new(inner)))
                    }
                    "Process" => {
                        self.expect_exact(TokenKind::Less, "`<`")?;
                        let input = self.parse_type_expr()?;
                        self.expect_exact(TokenKind::Comma, "`,`")?;
                        let output = self.parse_type_expr()?;
                        self.expect_exact(TokenKind::Greater, "`>`")?;
                        Ok(TypeExpr::Process {
                            input: Box::new(input),
                            output: Box::new(output),
                            input_count: 1,
                        })
                    }
                    "TriggerHandle" => {
                        self.expect_exact(TokenKind::Less, "`<`")?;
                        let event = self.parse_type_expr()?;
                        self.expect_exact(TokenKind::Greater, "`>`")?;
                        Ok(TypeExpr::TriggerHandle(Box::new(event)))
                    }
                    "Type" => self.parse_type_object(),
                    _ => Ok(TypeExpr::Ref(name)),
                }
            }
            _ => Err(ParseError::Expected {
                expected: "type expression",
                found: render_kind(&token.kind),
                span: token.span,
            }),
        }
    }

    fn expect_string_literal(&mut self) -> Result<AstString, ParseError> {
        let token = self.bump().clone();
        match token.kind {
            TokenKind::String(value) => Ok(value),
            other => Err(ParseError::Expected {
                expected: "string literal",
                found: render_kind(&other),
                span: token.span,
            }),
        }
    }

    fn expect_ident(&mut self) -> Result<AstString, ParseError> {
        let token = self.bump();
        match &token.kind {
            TokenKind::Ident(name) => Ok(name.clone()),
            other => Err(ParseError::Expected {
                expected: "identifier",
                found: render_kind(other),
                span: token.span,
            }),
        }
    }

    fn parse_type_name(&mut self) -> Result<AstString, ParseError> {
        let mut path = vec![self.expect_ident()?];
        while matches!(self.peek_kind(), TokenKind::Dot) {
            self.bump();
            path.push(self.expect_ident()?);
        }
        Ok(path
            .iter()
            .map(AstString::as_str)
            .collect::<Vec<_>>()
            .join(".")
            .into())
    }

    fn expect_key_name(&mut self) -> Result<AstString, ParseError> {
        let token = self.bump();
        match &token.kind {
            TokenKind::Ident(name) | TokenKind::String(name) => Ok(name.clone()),
            other => keyword_key_name(other)
                .map(Into::into)
                .ok_or_else(|| ParseError::Expected {
                    expected: "identifier, string key, or keyword key",
                    found: render_kind(other),
                    span: token.span,
                }),
        }
    }

    fn expect_exact(
        &mut self,
        expected_kind: TokenKind,
        expected: &'static str,
    ) -> Result<(), ParseError> {
        let token = self.bump();
        if std::mem::discriminant(&token.kind) == std::mem::discriminant(&expected_kind) {
            Ok(())
        } else {
            Err(ParseError::Expected {
                expected,
                found: render_kind(&token.kind),
                span: token.span,
            })
        }
    }

    fn unexpected(&mut self) -> ParseError {
        let token = self.peek();
        ParseError::Unexpected {
            found: render_kind(&token.kind),
            span: token.span,
        }
    }

    fn peek_assignment_target(&self) -> bool {
        if !matches!(self.peek_kind(), TokenKind::Ident(_)) {
            return false;
        }

        let mut index = self.index + 1;
        loop {
            match self.tokens.get(index).map(|token| &token.kind) {
                Some(TokenKind::Dot) => {
                    if !self
                        .tokens
                        .get(index + 1)
                        .is_some_and(|token| token_can_be_key(&token.kind))
                    {
                        return false;
                    }
                    index += 2;
                }
                Some(TokenKind::LBracket) => {
                    let Some(after_index) = self.skip_bracketed_index(index) else {
                        return false;
                    };
                    index = after_index;
                }
                Some(TokenKind::Equal) => return true,
                _ => return false,
            }
        }
    }

    fn skip_bracketed_index(&self, start: usize) -> Option<usize> {
        debug_assert!(matches!(
            self.tokens.get(start).map(|token| &token.kind),
            Some(TokenKind::LBracket)
        ));
        let mut parens = 0usize;
        let mut brackets = 1usize;
        let mut braces = 0usize;
        for (offset, token) in self.tokens.iter().enumerate().skip(start + 1) {
            match &token.kind {
                TokenKind::LParen => parens += 1,
                TokenKind::RParen => parens = parens.checked_sub(1)?,
                TokenKind::LBracket => brackets += 1,
                TokenKind::RBracket => {
                    brackets = brackets.checked_sub(1)?;
                    if brackets == 0 && parens == 0 && braces == 0 {
                        return Some(offset + 1);
                    }
                }
                TokenKind::LBrace => braces += 1,
                TokenKind::RBrace => braces = braces.checked_sub(1)?,
                TokenKind::Eof => return None,
                _ => {}
            }
        }
        None
    }

    fn question_starts_ternary(&self) -> bool {
        debug_assert!(matches!(self.peek_kind(), TokenKind::Question));
        let Some(next) = self.tokens.get(self.index + 1) else {
            return false;
        };
        if !token_can_start_expr(&next.kind) {
            return false;
        }

        let mut parens = 0usize;
        let mut brackets = 0usize;
        let mut braces = 0usize;
        for token in self.tokens.iter().skip(self.index + 1) {
            match &token.kind {
                TokenKind::Colon if parens == 0 && brackets == 0 && braces == 0 => return true,
                TokenKind::Equal if parens == 0 && brackets == 0 && braces == 0 => return false,
                TokenKind::Comma | TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace
                    if parens == 0 && brackets == 0 && braces == 0 =>
                {
                    return false;
                }
                TokenKind::Eof => return false,
                TokenKind::LParen => parens += 1,
                TokenKind::RParen => {
                    if parens == 0 {
                        return false;
                    }
                    parens -= 1;
                }
                TokenKind::LBracket => brackets += 1,
                TokenKind::RBracket => {
                    if brackets == 0 {
                        return false;
                    }
                    brackets -= 1;
                }
                TokenKind::LBrace => braces += 1,
                TokenKind::RBrace => {
                    if braces == 0 {
                        return false;
                    }
                    braces -= 1;
                }
                _ => {}
            }
        }
        false
    }

    fn paren_group_followed_by_lbrace(&self) -> bool {
        if !matches!(self.peek_kind(), TokenKind::LParen) {
            return false;
        }
        let mut depth = 0usize;
        for (index, token) in self.tokens.iter().enumerate().skip(self.index) {
            match &token.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self
                            .tokens
                            .get(index + 1)
                            .is_some_and(|next| matches!(next.kind, TokenKind::LBrace));
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
        }
        false
    }

    fn peek_contextual(&self, keyword: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(name) if name.as_str() == keyword)
    }

    fn expect_contextual(&mut self, keyword: &'static str) -> Result<(), ParseError> {
        let token = self.bump();
        match &token.kind {
            TokenKind::Ident(name) if name.as_str() == keyword => Ok(()),
            other => Err(ParseError::Expected {
                expected: keyword,
                found: render_kind(other),
                span: token.span,
            }),
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.index].kind
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn bump(&mut self) -> &Token {
        let token = &self.tokens[self.index];
        self.index += 1;
        token
    }
}

fn static_signal_name_arg(expr: &Expr, call: &'static str) -> Result<AstString, ParseError> {
    if let Expr::String(name) = expr {
        return Ok(name.clone());
    }
    Err(ParseError::Unexpected {
        found: format!("non-literal signal name in `{call}`"),
        span: Span { start: 0, end: 0 },
    })
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

include!("parser/tests.rs");
