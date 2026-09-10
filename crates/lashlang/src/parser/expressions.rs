use super::*;

impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<ParsedExpr, ParseError> {
        // Every nested expression (parenthesised, list/record element, index,
        // ternary, ...) funnels through here, so one depth guard at this
        // chokepoint bounds total recursive-descent stack growth.
        self.enter_nesting()?;
        let result = self.parse_tuple_expr();
        self.leave_nesting();
        result
    }

    pub(super) fn parse_expr_no_tuple(&mut self) -> Result<ParsedExpr, ParseError> {
        self.enter_nesting()?;
        let result = self.parse_ternary();
        self.leave_nesting();
        result
    }

    pub(super) fn parse_tuple_expr(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_ternary(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_or(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_and(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_compare(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_add(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_mul(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_unary(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_postfix(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_primary(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_list(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_list_comprehension_clauses(
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

    pub(super) fn parse_record(&mut self) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_record_entries(
        &mut self,
    ) -> Result<Vec<(AstString, ParsedExpr)>, ParseError> {
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

    pub(super) fn parse_call_arguments(&mut self) -> Result<Vec<ParsedExpr>, ParseError> {
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

    pub(super) fn parse_named_arguments(
        &mut self,
    ) -> Result<Vec<(AstString, ParsedExpr)>, ParseError> {
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

    pub(super) fn parse_process_start_expr(
        &mut self,
        start: usize,
    ) -> Result<ParsedExpr, ParseError> {
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

    pub(super) fn parse_sleep_expr(&mut self, start: usize) -> Result<ParsedExpr, ParseError> {
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
}
