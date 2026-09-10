use super::*;

impl Parser {
    pub(super) fn parse_program(&mut self) -> Result<Program, ParseError> {
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
                    || (self.peek_contextual("fn") && !self.peek_assignment_target())
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
            if self.peek_contextual("fn") && !self.peek_assignment_target() {
                let start = self.peek().span.start;
                declarations.push(Declaration::Function(self.parse_function_decl()?));
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

    pub(super) fn span_from(&self, start: usize) -> Span {
        let end = self
            .tokens
            .get(self.index.saturating_sub(1))
            .map(|token| token.span.end)
            .unwrap_or(start);
        Span { start, end }
    }

    pub(super) fn parse_type_decl(&mut self) -> Result<TypeDecl, ParseError> {
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

    pub(super) fn parse_process_decl(
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

    /// Parses `fn name(param: TYPE, ...) -> TYPE { body }`.
    ///
    /// Both the parameter types and the return type are mandatory, unlike
    /// `process`, whose return type may be inferred. A function is called from
    /// arbitrary sites and its result flows into typed positions, so an
    /// inferred signature would make the one thing a reader needs — what the
    /// call yields — invisible at the declaration. A mandatory return type also
    /// makes recursion typeable without a fixpoint: a recursive call is checked
    /// against the declared signature.
    pub(super) fn parse_function_decl(&mut self) -> Result<FunctionDecl, ParseError> {
        self.expect_contextual("fn")?;
        let name = self.expect_ident()?;
        self.expect_exact(TokenKind::LParen, "`(`")?;
        let mut params = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            let param_name = self.expect_ident()?;
            self.expect_exact(TokenKind::Colon, "`:`")?;
            let ty = self.parse_type_expr()?;
            params.push(FunctionParam {
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
        if !(matches!(self.peek_kind(), TokenKind::Minus)
            && self
                .tokens
                .get(self.index + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Greater)))
        {
            return Err(ParseError::Expected {
                expected: "`->` and a return type",
                found: render_kind(self.peek_kind()),
                span: self.peek().span,
            });
        }
        self.bump();
        self.bump();
        let return_ty = self.parse_type_expr()?;
        let body = self.parse_block()?;
        Ok(FunctionDecl {
            name,
            params,
            return_ty,
            body: body.into_expr(),
        })
    }

    pub(super) fn parse_process_signal_decls(
        &mut self,
    ) -> Result<Vec<ProcessSignalDecl>, ParseError> {
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

    pub(super) fn parse_statement_expr(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_let_assign(&mut self) -> Result<ParsedExpr, ParseError> {
        self.bump();
        self.parse_assign()
    }

    pub(super) fn parse_assign(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_assignment_target(&mut self) -> Result<ParsedAssignTarget, ParseError> {
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

    pub(super) fn parse_if(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_for(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_while(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_loop_control(
        &mut self,
        keyword: &'static str,
    ) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_finish(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_print(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_processes(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_cancel(&mut self) -> Result<ParsedExpr, ParseError> {
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
    pub(super) fn enter_nesting(&mut self) -> Result<(), ParseError> {
        if self.nesting_depth >= MAX_NESTING_DEPTH {
            return Err(ParseError::NestingTooDeep {
                limit: MAX_NESTING_DEPTH,
                span: self.peek().span,
            });
        }
        self.nesting_depth += 1;
        Ok(())
    }

    pub(super) fn leave_nesting(&mut self) {
        self.nesting_depth -= 1;
    }

    pub(super) fn parse_block(&mut self) -> Result<ParsedExpr, ParseError> {
        // Nested blocks (`if`/`for` bodies, bare braces) recurse through here
        // without passing through `parse_expr`, so they need their own guard.
        self.enter_nesting()?;
        let result = self.parse_block_inner();
        self.leave_nesting();
        result
    }

    pub(super) fn parse_block_inner(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_annotated_statement(&mut self) -> Result<ParsedExpr, ParseError> {
        let start = self.peek().span.start;
        let label = self.parse_label_annotation()?;
        if matches!(self.peek_kind(), TokenKind::At)
            || self.peek_contextual("type")
            || self.peek_contextual("process")
            || (self.peek_contextual("fn") && !self.peek_assignment_target())
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

    pub(super) fn parse_label_annotation(&mut self) -> Result<LabelMetadata, ParseError> {
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
}
