use super::*;

impl Parser {
    pub(super) fn parse_type_object(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        self.parse_type_object_body_after_lbrace()
    }

    pub(super) fn parse_type_object_body(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect_exact(TokenKind::LBrace, "`{`")?;
        self.parse_type_object_body_after_lbrace()
    }

    pub(super) fn parse_type_object_body_after_lbrace(&mut self) -> Result<TypeExpr, ParseError> {
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

    pub(super) fn parse_type_expr(&mut self) -> Result<TypeExpr, ParseError> {
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

    pub(super) fn parse_type_term(&mut self) -> Result<TypeExpr, ParseError> {
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

    pub(super) fn expect_string_literal(&mut self) -> Result<AstString, ParseError> {
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

    pub(super) fn expect_ident(&mut self) -> Result<AstString, ParseError> {
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

    pub(super) fn parse_type_name(&mut self) -> Result<AstString, ParseError> {
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

    pub(super) fn expect_key_name(&mut self) -> Result<AstString, ParseError> {
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

    pub(super) fn expect_exact(
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

    pub(super) fn unexpected(&mut self) -> ParseError {
        let token = self.peek();
        ParseError::Unexpected {
            found: render_kind(&token.kind),
            span: token.span,
        }
    }

    pub(super) fn peek_assignment_target(&self) -> bool {
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

    pub(super) fn skip_bracketed_index(&self, start: usize) -> Option<usize> {
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

    pub(super) fn question_starts_ternary(&self) -> bool {
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

    pub(super) fn paren_group_followed_by_lbrace(&self) -> bool {
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

    pub(super) fn peek_contextual(&self, keyword: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(name) if name.as_str() == keyword)
    }

    pub(super) fn expect_contextual(&mut self, keyword: &'static str) -> Result<(), ParseError> {
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

    pub(super) fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    pub(super) fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.index].kind
    }

    pub(super) fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }

    pub(super) fn bump(&mut self) -> &Token {
        let token = &self.tokens[self.index];
        self.index += 1;
        token
    }
}
