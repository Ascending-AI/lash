//! The recursive-descent parser over lexed `CHECK` expressions: `OR`, `AND`,
//! `NOT`, comparisons, membership lists and parenthesised groups, into the
//! normalized [`Expr`] the backends' renderings are compared as.

use super::{Comparison, Expr, SqlIdentifier, Token, TokenKind};

pub(super) struct Parser {
    pub(super) tokens: Vec<Token>,
    pub(super) index: usize,
}

impl Parser {
    pub(super) fn parse_or(&mut self) -> Result<Expr, String> {
        let mut expression = self.parse_and()?;
        while self.consume_ident("or") {
            expression = Expr::Or(Box::new(expression), Box::new(self.parse_and()?));
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut expression = self.parse_not()?;
        while self.consume_ident("and") {
            expression = Expr::And(Box::new(expression), Box::new(self.parse_not()?));
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.consume_ident("not") {
            return Ok(Expr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Expr, String> {
        let left = self.parse_value()?;
        if self.consume_ident("is") {
            let negated = self.consume_ident("not");
            self.expect_ident("null")?;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        if self.consume_ident("in") {
            self.expect(TokenKind::LParen)?;
            let values = self.parse_list(TokenKind::RParen)?;
            return Ok(Expr::In(Box::new(left), values));
        }
        let Some(TokenKind::Comparison(comparison)) = self.peek().map(|token| token.kind.clone())
        else {
            return Ok(left);
        };
        self.index += 1;
        if comparison == Comparison::Equal && self.consume_ident("any") {
            self.expect(TokenKind::LParen)?;
            self.expect_ident("array")?;
            self.expect(TokenKind::LBracket)?;
            let values = self.parse_list(TokenKind::RBracket)?;
            self.expect(TokenKind::RParen)?;
            return Ok(Expr::In(Box::new(left), values));
        }
        Ok(Expr::Compare(
            Box::new(left),
            comparison,
            Box::new(self.parse_value()?),
        ))
    }

    fn parse_list(&mut self, closing: TokenKind) -> Result<Vec<Expr>, String> {
        let mut values = Vec::new();
        if self.consume(closing.clone()) {
            return Ok(values);
        }
        loop {
            values.push(self.parse_value()?);
            if self.consume(closing.clone()) {
                return Ok(values);
            }
            self.expect(TokenKind::Comma)?;
        }
    }

    fn parse_value(&mut self) -> Result<Expr, String> {
        let mut value = if self.consume(TokenKind::LParen) {
            let value = self.parse_or()?;
            self.expect(TokenKind::RParen)?;
            value
        } else {
            let token = self
                .tokens
                .get(self.index)
                .cloned()
                .ok_or_else(|| "expected expression, found end of input".to_string())?;
            self.index += 1;
            match token.kind {
                TokenKind::Ident(identifier) if identifier == "true" => Expr::Boolean(true),
                TokenKind::Ident(identifier) if identifier == "false" => Expr::Boolean(false),
                TokenKind::Ident(identifier) => {
                    let identifier = SqlIdentifier::unquoted(identifier);
                    if self.consume(TokenKind::LParen) {
                        let arguments = self.parse_list(TokenKind::RParen)?;
                        Expr::Call(identifier, arguments)
                    } else {
                        Expr::Identifier(identifier)
                    }
                }
                TokenKind::QuotedIdent(identifier) => {
                    let identifier = SqlIdentifier::quoted(identifier);
                    if self.consume(TokenKind::LParen) {
                        let arguments = self.parse_list(TokenKind::RParen)?;
                        Expr::Call(identifier, arguments)
                    } else {
                        Expr::Identifier(identifier)
                    }
                }
                TokenKind::String(value) => Expr::String(value),
                TokenKind::Number(value) => Expr::Number(value),
                _ => {
                    return Err(format!(
                        "unsupported expression token at byte {}",
                        token.start
                    ));
                }
            }
        };
        loop {
            if self.consume(TokenKind::Cast) {
                let cast_token = self
                    .tokens
                    .get(self.index)
                    .map(|token| token.kind.clone())
                    .ok_or_else(|| "expected cast type".to_string())?;
                self.index += 1;
                match cast_token {
                    TokenKind::Ident(cast)
                        if cast == "text" && matches!(value, Expr::String(_)) => {}
                    TokenKind::Ident(cast) => {
                        value = Expr::Cast(Box::new(value), SqlIdentifier::unquoted(cast));
                    }
                    TokenKind::QuotedIdent(cast) => {
                        value = Expr::Cast(Box::new(value), SqlIdentifier::Exact(cast));
                    }
                    _ => return Err("expected cast type".to_string()),
                }
            } else if self.consume(TokenKind::JsonText) {
                value = Expr::JsonText(Box::new(value), Box::new(self.parse_value()?));
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn consume_ident(&mut self, identifier: &str) -> bool {
        if self.peek().is_some_and(|token| token.is_ident(identifier)) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self, identifier: &str) -> Result<(), String> {
        if self.consume_ident(identifier) {
            Ok(())
        } else {
            Err(format!("expected `{identifier}`"))
        }
    }

    fn consume(&mut self, kind: TokenKind) -> bool {
        if self.peek().is_some_and(|token| token.kind == kind) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<(), String> {
        if self.consume(kind.clone()) {
            Ok(())
        } else {
            Err(format!("expected {kind:?}"))
        }
    }
}
