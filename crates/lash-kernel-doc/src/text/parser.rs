//! Reads kernel text.

use std::collections::{BTreeMap, BTreeSet};

use super::lexer::{Spanned, Token, lex};
use super::{ParseError, ParseErrorReason, is_keyword};
use crate::ast::{
    Action, Atom, Block, Callee, Catch, Closure, Expr, Function, JoinMode, Literal, MapEntry,
    Member, Place, ProjectionRead, RecordEntry, Rhs, Stmt, TryStmt,
};
use crate::document::{Document, MAX_NESTING_DEPTH, Manifest};
use crate::function::{Formula, FunctionBody, FunctionDefinition, Guard, Implementation, Operand};
use crate::name::{FunctionId, FunctionName, Name, QualifiedName};
use crate::number::{Float, NumberPolicy};
use crate::types::{MapType, Param, RecordType, RecordTypeField, Signature, Type};
use crate::validate::StatementForm;
use crate::value::Bytes;

/// Reads a document from kernel text.
pub fn parse_document(text: &str) -> Result<Document, ParseError> {
    let mut parser = Parser::new(text)?;
    let mut kernel = None;
    let mut numbers = None;
    let mut main = None;
    let mut effects = BTreeMap::new();
    let mut entries = BTreeMap::new();
    let mut private_bindings = BTreeSet::new();
    // Bodies are parsed after every `use` line is known, so a function may
    // be used above the line that names it.
    let mut bodies: Vec<(Option<Name>, Vec<Name>, usize)> = Vec::new();
    while parser.peek() != &Token::End {
        let word = parser.keyword("a top-level item")?;
        match word.as_str() {
            "kernel" => {
                let version = parser.version()?;
                parser.once(kernel.replace(version), "`kernel`")?;
            }
            "numbers" => {
                let policy = match parser.keyword("`float` or `by_spelling`")?.as_str() {
                    "float" => NumberPolicy::Float,
                    "by_spelling" => NumberPolicy::BySpelling,
                    _ => return Err(parser.unexpected_previous("`float` or `by_spelling`")),
                };
                parser.once(numbers.replace(policy), "`numbers`")?;
            }
            "effect" => {
                let name = parser.qualified_name()?;
                let signature = parser.signature()?;
                parser.once(
                    effects.insert(name.clone(), signature),
                    &format!("effect `{name}`"),
                )?;
            }
            "use" => parser.use_line()?,
            "entry" => {
                let name = parser.name()?;
                let signature = parser.signature()?;
                parser.once(
                    entries.insert(name.clone(), signature),
                    &format!("entry `{name}`"),
                )?;
            }
            "private" => loop {
                private_bindings.insert(parser.name()?);
                if !parser.eat(&Token::Comma) {
                    break;
                }
            },
            "fn" => {
                let name = parser.name()?;
                let params = parser.names()?;
                bodies.push((Some(name), params, parser.position));
                parser.skip_block()?;
            }
            "main" => {
                bodies.push((None, Vec::new(), parser.position));
                parser.skip_block()?;
            }
            _ => return Err(parser.unexpected_previous("a top-level item")),
        }
    }
    let end = parser.position;
    let mut functions = BTreeMap::new();
    for (name, params, start) in bodies {
        parser.position = start;
        let body = parser.block()?;
        match name {
            Some(name) => {
                let item = format!("function `{name}`");
                parser.once(functions.insert(name, Function { params, body }), &item)?;
            }
            None => parser.once(main.replace(body), "`main`")?,
        }
    }
    parser.position = end;
    Ok(Document {
        manifest: Manifest {
            kernel: kernel.ok_or_else(|| parser.missing("the `kernel` line"))?,
            effects,
            functions: std::mem::take(&mut parser.uses),
            numbers: numbers.ok_or_else(|| parser.missing("the `numbers` line"))?,
        },
        functions,
        entries,
        private_bindings,
        main: main.ok_or_else(|| parser.missing("`main`"))?,
    })
}

/// Reads a library-function definition from kernel text.
pub fn parse_definition(text: &str) -> Result<FunctionDefinition, ParseError> {
    let mut parser = Parser::new(text)?;
    parser.expect_keyword("function")?;
    let name = parser.qualified_name()?;
    let signature = parser.signature()?;
    let mut kernel = None;
    let mut errors = BTreeSet::new();
    let mut charge = None;
    let mut guard = None;
    let mut native = false;
    let mut native_version = None;
    let mut body_start = None;
    while parser.peek() != &Token::End {
        let word = parser.keyword("a definition item")?;
        match word.as_str() {
            "kernel" => {
                let version = parser.version()?;
                parser.once(kernel.replace(version), "`kernel`")?;
            }
            "errors" => loop {
                errors.insert(parser.text()?);
                if !parser.eat(&Token::Comma) {
                    break;
                }
            },
            "charge" => {
                let formula = parser.formula()?;
                parser.once(charge.replace(formula), "`charge`")?;
            }
            "guard" => {
                let unit = parser.text()?;
                let limit = parser.formula()?;
                parser.once(guard.replace(Guard { unit, limit }), "`guard`")?;
            }
            "native" => {
                native = true;
                if matches!(parser.peek(), Token::Int(_)) {
                    let version = parser.version()?;
                    parser.once(native_version.replace(version), "the native version")?;
                }
            }
            "use" => parser.use_line()?,
            "body" => {
                parser.once(body_start.replace(parser.position), "`body`")?;
                parser.skip_block()?;
            }
            _ => return Err(parser.unexpected_previous("a definition item")),
        }
    }
    let body = match body_start {
        Some(start) => {
            parser.position = start;
            let block = parser.block()?;
            Some(FunctionBody {
                functions: std::mem::take(&mut parser.uses),
                block,
            })
        }
        None => None,
    };
    let implementation = match (native, body) {
        (true, None) => Implementation::Native,
        (false, Some(body)) => Implementation::Body(body),
        (true, Some(body)) => Implementation::Both(body),
        (false, None) => return Err(parser.missing("`native`, a `body`, or both")),
    };
    Ok(FunctionDefinition {
        kernel: kernel.ok_or_else(|| parser.missing("the `kernel` line"))?,
        name,
        signature,
        errors,
        charge: charge.ok_or_else(|| parser.missing("the `charge` line"))?,
        guard,
        implementation,
        native_version: native_version.unwrap_or(crate::FIRST_NATIVE_VERSION),
    })
}

struct Parser {
    tokens: Vec<Spanned>,
    position: usize,
    /// The `use` lines read so far.
    uses: BTreeMap<FunctionId, FunctionName>,
    depth: usize,
}

type Parsed<T> = Result<T, ParseError>;

impl Parser {
    fn new(text: &str) -> Parsed<Self> {
        Ok(Self {
            tokens: lex(text)?,
            position: 0,
            uses: BTreeMap::new(),
            depth: 0,
        })
    }

    fn spanned(&self, position: usize) -> &Spanned {
        // The token list always ends with `Token::End`, and nothing advances
        // past it.
        &self.tokens[position.min(self.tokens.len() - 1)]
    }

    fn peek(&self) -> &Token {
        &self.spanned(self.position).token
    }

    fn peek_at(&self, offset: usize) -> &Token {
        &self.spanned(self.position + offset).token
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if token != Token::End {
            self.position += 1;
        }
        token
    }

    fn error_at(&self, position: usize, reason: ParseErrorReason) -> ParseError {
        let spanned = self.spanned(position);
        ParseError {
            line: spanned.line,
            column: spanned.column,
            reason,
        }
    }

    fn error(&self, reason: ParseErrorReason) -> ParseError {
        self.error_at(self.position, reason)
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        self.error(ParseErrorReason::Unexpected {
            found: self.peek().describe(),
            expected: expected.to_string(),
        })
    }

    /// The same, for the token just consumed.
    fn unexpected_previous(&self, expected: &str) -> ParseError {
        let position = self.position.saturating_sub(1);
        self.error_at(
            position,
            ParseErrorReason::Unexpected {
                found: self.spanned(position).token.describe(),
                expected: expected.to_string(),
            },
        )
    }

    fn missing(&self, item: &'static str) -> ParseError {
        self.error(ParseErrorReason::Missing { item })
    }

    fn once<T>(&self, previous: Option<T>, item: &str) -> Parsed<()> {
        match previous {
            None => Ok(()),
            Some(_) => Err(self.error_at(
                self.position.saturating_sub(1),
                ParseErrorReason::Duplicate {
                    item: item.to_string(),
                },
            )),
        }
    }

    fn eat(&mut self, token: &Token) -> bool {
        let found = self.peek() == token;
        if found {
            self.position += 1;
        }
        found
    }

    fn expect(&mut self, token: &Token) -> Parsed<()> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(self.unexpected(&token.describe()))
        }
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Word(found) if found == word)
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = self.is_word(word);
        if found {
            self.position += 1;
        }
        found
    }

    fn expect_keyword(&mut self, word: &str) -> Parsed<()> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{word}`")))
        }
    }

    /// Any bare word.
    fn keyword(&mut self, expected: &str) -> Parsed<String> {
        match self.peek() {
            Token::Word(_) => match self.advance() {
                Token::Word(word) => Ok(word),
                _ => Err(self.unexpected(expected)),
            },
            _ => Err(self.unexpected(expected)),
        }
    }

    /// A name: a bare word that is no keyword, or any text between
    /// backticks.
    fn name(&mut self) -> Parsed<Name> {
        match self.peek() {
            Token::Word(word) if !is_keyword(word) => {}
            Token::Quoted(_) => {}
            _ => return Err(self.unexpected("a name")),
        }
        match self.advance() {
            Token::Word(name) | Token::Quoted(name) => Ok(Name::new(name)),
            _ => Err(self.unexpected("a name")),
        }
    }

    fn names(&mut self) -> Parsed<Vec<Name>> {
        self.expect(&Token::LParen)?;
        let mut names = Vec::new();
        while !self.eat(&Token::RParen) {
            names.push(self.name()?);
            if !self.eat(&Token::Comma) {
                self.expect(&Token::RParen)?;
                break;
            }
        }
        Ok(names)
    }

    /// Words joined by `.`. A segment may be spelled like a keyword.
    fn qualified_name(&mut self) -> Parsed<QualifiedName> {
        let start = self.position;
        let mut text = self.keyword("a qualified name")?;
        while self.peek() == &Token::Dot && matches!(self.peek_at(1), Token::Word(_)) {
            self.position += 1;
            text.push('.');
            text.push_str(&self.keyword("a qualified name")?);
        }
        QualifiedName::new(text).map_err(|error| {
            self.error_at(
                start,
                ParseErrorReason::Malformed {
                    problem: error.to_string(),
                },
            )
        })
    }

    fn text(&mut self) -> Parsed<String> {
        match self.advance() {
            Token::Text(text) => Ok(text),
            _ => Err(self.unexpected_previous("a text")),
        }
    }

    /// A field name: any bare word, or a text.
    fn field_name(&mut self) -> Parsed<String> {
        match self.advance() {
            Token::Word(name) | Token::Text(name) => Ok(name),
            _ => Err(self.unexpected_previous("a field name")),
        }
    }

    fn version(&mut self) -> Parsed<u32> {
        match self.advance() {
            Token::Int(version) => u32::try_from(version.as_bigint())
                .map_err(|_| self.unexpected_previous("a kernel version")),
            _ => Err(self.unexpected_previous("a kernel version")),
        }
    }

    fn use_line(&mut self) -> Parsed<()> {
        let name = self.qualified_name()?;
        self.expect(&Token::Equals)?;
        let Token::Identity(function) = self.advance() else {
            return Err(self.unexpected_previous("an identity, as `@<64 hex digits>`"));
        };
        let previous = self.uses.insert(function, name);
        self.once(previous, &format!("`use` of @{function}"))
    }

    /// Passes over a balanced `{ … }` without reading it.
    fn skip_block(&mut self) -> Parsed<()> {
        self.expect(&Token::LBrace)?;
        let mut open = 1usize;
        while open > 0 {
            match self.advance() {
                Token::LBrace => open += 1,
                Token::RBrace => open -= 1,
                Token::End => return Err(self.unexpected("`}`")),
                _ => {}
            }
        }
        Ok(())
    }

    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Parsed<T>) -> Parsed<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.error(ParseErrorReason::TooDeep {
                limit: MAX_NESTING_DEPTH,
            }));
        }
        self.depth += 1;
        let parsed = parse(self);
        self.depth -= 1;
        parsed
    }

    fn block(&mut self) -> Parsed<Block> {
        self.nested(|parser| {
            parser.expect(&Token::LBrace)?;
            let mut block = Vec::new();
            while !parser.eat(&Token::RBrace) {
                block.push(parser.nested(Self::stmt)?);
            }
            Ok(block)
        })
    }

    fn stmt(&mut self) -> Parsed<Stmt> {
        let word = self.keyword("a statement")?;
        Ok(match word.as_str() {
            "let" => {
                let name = self.name()?;
                self.expect(&Token::Equals)?;
                Stmt::Let {
                    name,
                    value: self.rhs()?,
                }
            }
            "set" => {
                let start = self.position;
                let place = match self.expr()? {
                    Expr::Variable(name) => Place::Variable(name),
                    Expr::Member(member) => Place::Member(*member),
                    _ => {
                        return Err(self.error_at(
                            start,
                            ParseErrorReason::Malformed {
                                problem: "`set` writes a variable, a field or an index".to_string(),
                            },
                        ));
                    }
                };
                self.expect(&Token::Equals)?;
                Stmt::Assign {
                    place,
                    value: self.rhs()?,
                }
            }
            "remove" => {
                let start = self.position;
                match self.expr()? {
                    Expr::Member(member) => Stmt::Remove { member: *member },
                    _ => {
                        return Err(self.error_at(
                            start,
                            ParseErrorReason::Malformed {
                                problem: "`remove` takes a field or an index".to_string(),
                            },
                        ));
                    }
                }
            }
            "do" => match self.action()? {
                Some(action) => Stmt::Do { action },
                None => return Err(self.unexpected("an action")),
            },
            "if" => {
                let condition = self.expr()?;
                let then_block = self.block()?;
                let else_block = if self.eat_word("else") {
                    self.block()?
                } else {
                    Vec::new()
                };
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                }
            }
            "for" => {
                let binding = self.name()?;
                self.expect_keyword("in")?;
                Stmt::For {
                    binding,
                    iterable: self.expr()?,
                    body: self.block()?,
                }
            }
            "while" => Stmt::While {
                condition: self.expr()?,
                body: self.block()?,
            },
            "break" => Stmt::Break,
            "continue" => Stmt::Continue,
            "return" => Stmt::Return {
                value: self.expr()?,
            },
            "try" => {
                let body = self.block()?;
                let catch = if self.eat_word("catch") {
                    Some(Catch {
                        binding: self.name()?,
                        body: self.block()?,
                    })
                } else {
                    None
                };
                let finally = if self.eat_word("finally") {
                    Some(self.block()?)
                } else {
                    None
                };
                Stmt::Try(TryStmt {
                    body,
                    catch,
                    finally,
                })
            }
            "throw" => Stmt::Throw {
                value: self.expr()?,
            },
            "print" => Stmt::Print {
                value: self.expr()?,
            },
            "finish" => Stmt::Finish {
                value: self.expr()?,
            },
            "fail" => Stmt::Fail {
                value: self.expr()?,
            },
            _ => return Err(self.unexpected_previous("a statement")),
        })
    }

    fn rhs(&mut self) -> Parsed<Rhs> {
        Ok(match self.action()? {
            Some(action) => Rhs::Action(action),
            None => Rhs::Expr(self.expr()?),
        })
    }

    /// The statement form the next word begins, if it begins one.
    fn statement_form(&self) -> Option<StatementForm> {
        let Token::Word(word) = self.peek() else {
            return None;
        };
        Some(match word.as_str() {
            "call" | "apply" | "invoke" => StatementForm::Call,
            "perform" => StatementForm::Perform,
            "sleep" => StatementForm::Sleep,
            "join" => StatementForm::Join,
            "yield" => StatementForm::Yield,
            "spawn" => StatementForm::Spawn,
            "cancel" => StatementForm::Cancel,
            _ => return None,
        })
    }

    fn action(&mut self) -> Parsed<Option<Action>> {
        // A library function may be named like a statement form
        // (`join.all(…)`); a call by name is an expression.
        if self.at_named_call() {
            return Ok(None);
        }
        let Some(form) = self.statement_form() else {
            return Ok(None);
        };
        Ok(Some(match form {
            StatementForm::Call => {
                let (callee, args) = self.callee()?;
                Action::Call { callee, args }
            }
            StatementForm::Perform => {
                self.position += 1;
                let effect = self.qualified_name()?;
                let args = self.atoms()?;
                self.expect_keyword("as")?;
                Action::Perform {
                    effect,
                    args,
                    result: self.ty()?,
                }
            }
            StatementForm::Sleep => {
                self.position += 1;
                Action::Sleep {
                    duration: self.atom()?,
                }
            }
            StatementForm::Join => {
                self.position += 1;
                let mode = match self.peek() {
                    Token::Word(word) => match word.as_str() {
                        "all" => Some(JoinMode::All),
                        "settled" => Some(JoinMode::AllSettled),
                        "race" => Some(JoinMode::Race),
                        "any" => Some(JoinMode::Any),
                        _ => None,
                    },
                    _ => None,
                };
                match mode {
                    Some(mode) => {
                        self.position += 1;
                        Action::JoinMany {
                            mode,
                            tasks: self.atom()?,
                        }
                    }
                    None => Action::Join { task: self.atom()? },
                }
            }
            StatementForm::Yield => {
                self.position += 1;
                Action::Yield
            }
            StatementForm::Spawn => {
                self.position += 1;
                let (callee, args) = self.callee()?;
                Action::Spawn { callee, args }
            }
            StatementForm::Cancel => {
                self.position += 1;
                Action::Cancel { task: self.atom()? }
            }
        }))
    }

    fn callee(&mut self) -> Parsed<(Callee, Vec<Atom>)> {
        let callee = match self.keyword("`call`, `apply` or `invoke`")?.as_str() {
            "call" => Callee::Declared(self.name()?),
            "apply" => Callee::Value(self.name()?),
            "invoke" => Callee::Library(self.function_reference()?),
            _ => return Err(self.unexpected_previous("`call`, `apply` or `invoke`")),
        };
        Ok((callee, self.atoms()?))
    }

    /// A library function: `@<hex>`, or the name one `use` line gives.
    fn function_reference(&mut self) -> Parsed<FunctionId> {
        if let Token::Identity(function) = self.peek() {
            let function = *function;
            self.position += 1;
            return Ok(function);
        }
        let start = self.position;
        let name = self.qualified_name()?;
        let mut named = self
            .uses
            .iter()
            .filter(|(_, used)| **used == name)
            .map(|(function, _)| *function);
        let name = name.to_string();
        match (named.next(), named.next()) {
            (Some(function), None) => Ok(function),
            (None, _) => Err(self.error_at(start, ParseErrorReason::UnknownFunction { name })),
            (Some(_), Some(_)) => {
                Err(self.error_at(start, ParseErrorReason::AmbiguousFunction { name }))
            }
        }
    }

    fn atoms(&mut self) -> Parsed<Vec<Atom>> {
        self.expect(&Token::LParen)?;
        let mut atoms = Vec::new();
        while !self.eat(&Token::RParen) {
            atoms.push(self.atom()?);
            if !self.eat(&Token::Comma) {
                if self.peek() != &Token::RParen {
                    return Err(self.error(ParseErrorReason::ArgumentNotAtom));
                }
                self.position += 1;
                break;
            }
        }
        Ok(atoms)
    }

    fn atom(&mut self) -> Parsed<Atom> {
        if let Some(form) = self.statement_form() {
            return Err(self.error(ParseErrorReason::StatementForm { form }));
        }
        if let Some(literal) = self.literal()? {
            return Ok(Atom::Literal(literal));
        }
        match self.peek() {
            Token::Word(word) if !is_keyword(word) => {}
            Token::Quoted(_) => {}
            _ => return Err(self.error(ParseErrorReason::ArgumentNotAtom)),
        }
        Ok(Atom::Variable(self.name()?))
    }

    fn literal(&mut self) -> Parsed<Option<Literal>> {
        let literal = match self.peek() {
            Token::Word(word) => match word.as_str() {
                "null" => Literal::Null,
                "absent" => Literal::Absent,
                "true" => Literal::Bool(true),
                "false" => Literal::Bool(false),
                "nan" => Literal::Float(Float::new(f64::NAN)),
                "inf" => Literal::Float(Float::new(f64::INFINITY)),
                _ => return Ok(None),
            },
            Token::Int(value) => Literal::Int(value.clone()),
            Token::Float(value) => Literal::Float(*value),
            Token::Text(text) => Literal::Text(text.clone()),
            Token::Bytes(bytes) => Literal::Bytes(Bytes::new(bytes.clone())),
            Token::Ampersand => {
                self.position += 1;
                return Ok(Some(Literal::Function(self.name()?)));
            }
            _ => return Ok(None),
        };
        self.position += 1;
        Ok(Some(literal))
    }

    /// Whether the tokens ahead spell `word(.word)* (`: a library call by
    /// name.
    fn at_named_call(&self) -> bool {
        if !matches!(self.peek(), Token::Word(_)) {
            return false;
        }
        let mut offset = 1;
        while self.peek_at(offset) == &Token::Dot
            && matches!(self.peek_at(offset + 1), Token::Word(_))
        {
            offset += 2;
        }
        let single = offset == 1;
        let reserved = single && (self.is_word("fn") || self.is_word("read"));
        self.peek_at(offset) == &Token::LParen && !reserved
    }

    fn exprs(&mut self, close: &Token) -> Parsed<Vec<Expr>> {
        let mut exprs = Vec::new();
        while !self.eat(close) {
            exprs.push(self.expr()?);
            if !self.eat(&Token::Comma) {
                self.expect(close)?;
                break;
            }
        }
        Ok(exprs)
    }

    fn expr(&mut self) -> Parsed<Expr> {
        self.nested(|parser| {
            let mut expr = parser.primary()?;
            let mut links = 0usize;
            loop {
                let member = if parser.eat(&Token::Dot) {
                    Member::Field {
                        target: expr,
                        field: parser.field_name()?,
                    }
                } else if parser.eat(&Token::LBracket) {
                    let index = parser.expr()?;
                    parser.expect(&Token::RBracket)?;
                    Member::Index {
                        target: expr,
                        index,
                    }
                } else {
                    return Ok(expr);
                };
                // Each link nests the tree one level without nesting the
                // parser, so it is counted here.
                links += 1;
                if parser.depth + links > MAX_NESTING_DEPTH {
                    return Err(parser.error(ParseErrorReason::TooDeep {
                        limit: MAX_NESTING_DEPTH,
                    }));
                }
                expr = Expr::Member(Box::new(member));
            }
        })
    }

    fn primary(&mut self) -> Parsed<Expr> {
        if self.at_named_call() {
            let function = self.function_reference()?;
            self.expect(&Token::LParen)?;
            return Ok(Expr::Call {
                function,
                args: self.exprs(&Token::RParen)?,
            });
        }
        if let Some(form) = self.statement_form() {
            return Err(self.error(ParseErrorReason::StatementForm { form }));
        }
        if let Some(literal) = self.literal()? {
            return Ok(Expr::Literal(literal));
        }
        match self.advance() {
            Token::Identity(function) => {
                self.expect(&Token::LParen)?;
                Ok(Expr::Call {
                    function,
                    args: self.exprs(&Token::RParen)?,
                })
            }
            Token::Quoted(name) => Ok(Expr::Variable(Name::new(name))),
            Token::Word(word) => match word.as_str() {
                "clock" => Ok(Expr::Clock),
                "random" => Ok(Expr::Random),
                "read" => {
                    self.expect(&Token::LParen)?;
                    let handle = self.expr()?;
                    self.expect(&Token::Comma)?;
                    let request = self.expr()?;
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Read(Box::new(ProjectionRead { handle, request })))
                }
                "fn" => Ok(Expr::Closure(Box::new(Closure {
                    params: self.names()?,
                    body: self.block()?,
                }))),
                "map" => {
                    self.expect(&Token::LBrace)?;
                    let mut entries = Vec::new();
                    while !self.eat(&Token::RBrace) {
                        let key = self.expr()?;
                        self.expect(&Token::Colon)?;
                        entries.push(MapEntry {
                            key,
                            value: self.expr()?,
                        });
                        if !self.eat(&Token::Comma) {
                            self.expect(&Token::RBrace)?;
                            break;
                        }
                    }
                    Ok(Expr::Map(entries))
                }
                "set" => {
                    self.expect(&Token::LBrace)?;
                    Ok(Expr::Set(self.exprs(&Token::RBrace)?))
                }
                word if is_keyword(word) => Err(self.unexpected_previous("an expression")),
                name => Ok(Expr::Variable(Name::new(name))),
            },
            Token::LBracket => Ok(Expr::List(self.exprs(&Token::RBracket)?)),
            Token::LBrace => {
                let mut entries = Vec::new();
                while !self.eat(&Token::RBrace) {
                    let field = self.field_name()?;
                    self.expect(&Token::Colon)?;
                    entries.push(RecordEntry {
                        field,
                        value: self.expr()?,
                    });
                    if !self.eat(&Token::Comma) {
                        self.expect(&Token::RBrace)?;
                        break;
                    }
                }
                Ok(Expr::Record(entries))
            }
            Token::LParen => {
                // `()`, `(a,)`, `(a, b)`. There is no grouping: `(a)` is
                // refused.
                let mut items = Vec::new();
                let mut comma = false;
                while !self.eat(&Token::RParen) {
                    items.push(self.expr()?);
                    comma = self.eat(&Token::Comma);
                    if !comma {
                        self.expect(&Token::RParen)?;
                        break;
                    }
                }
                if items.len() == 1 && !comma {
                    return Err(
                        self.unexpected_previous("`,`: a one-member tuple is written `(a,)`")
                    );
                }
                Ok(Expr::Tuple(items))
            }
            _ => Err(self.unexpected_previous("an expression")),
        }
    }

    fn signature(&mut self) -> Parsed<Signature> {
        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        while !self.eat(&Token::RParen) {
            let name = self.name()?;
            let optional = self.eat(&Token::Question);
            self.expect(&Token::Colon)?;
            params.push(Param {
                name,
                ty: self.ty()?,
                optional,
            });
            if !self.eat(&Token::Comma) {
                self.expect(&Token::RParen)?;
                break;
            }
        }
        self.expect(&Token::Arrow)?;
        Ok(Signature {
            params,
            result: self.ty()?,
        })
    }

    fn types(&mut self) -> Parsed<Vec<Type>> {
        self.expect(&Token::LParen)?;
        let mut types = Vec::new();
        while !self.eat(&Token::RParen) {
            types.push(self.ty()?);
            if !self.eat(&Token::Comma) {
                self.expect(&Token::RParen)?;
                break;
            }
        }
        Ok(types)
    }

    fn one_type(&mut self) -> Parsed<Type> {
        self.expect(&Token::LParen)?;
        let ty = self.ty()?;
        self.expect(&Token::RParen)?;
        Ok(ty)
    }

    fn ty(&mut self) -> Parsed<Type> {
        self.nested(|parser| {
            let word = parser.keyword("a type")?;
            Ok(match word.as_str() {
                "Any" => Type::Any,
                "Null" => Type::Null,
                "Absent" => Type::Absent,
                "Bool" => Type::Bool,
                "Int" => Type::Int,
                "Float" => Type::Float,
                "Number" => Type::Number,
                "Text" => Type::Text,
                "Bytes" => Type::Bytes,
                "Timestamp" => Type::Timestamp,
                "Error" => Type::Error,
                "Tuple" => Type::Tuple(parser.types()?),
                "Union" => Type::Union(parser.types()?),
                "List" => Type::List(Box::new(parser.one_type()?)),
                "Set" => Type::Set(Box::new(parser.one_type()?)),
                "Task" => Type::Task(Box::new(parser.one_type()?)),
                "Map" => {
                    parser.expect(&Token::LParen)?;
                    let key = parser.ty()?;
                    parser.expect(&Token::Comma)?;
                    let value = parser.ty()?;
                    parser.expect(&Token::RParen)?;
                    Type::Map(Box::new(MapType { key, value }))
                }
                "Handle" => {
                    parser.expect(&Token::LParen)?;
                    let kind = parser.text()?;
                    parser.expect(&Token::RParen)?;
                    Type::Handle(kind)
                }
                "Enum" => {
                    parser.expect(&Token::LParen)?;
                    let mut members = Vec::new();
                    while !parser.eat(&Token::RParen) {
                        members.push(parser.text()?);
                        if !parser.eat(&Token::Comma) {
                            parser.expect(&Token::RParen)?;
                            break;
                        }
                    }
                    Type::Enum(members)
                }
                "Fn" => Type::Function(Box::new(parser.signature()?)),
                "Record" => {
                    parser.expect(&Token::LBrace)?;
                    let mut record = RecordType {
                        fields: Vec::new(),
                        rest: None,
                    };
                    while !parser.eat(&Token::RBrace) {
                        if parser.eat(&Token::DotDot) {
                            record.rest = Some(Box::new(parser.ty()?));
                            parser.expect(&Token::RBrace)?;
                            break;
                        }
                        let name = parser.field_name()?;
                        let optional = parser.eat(&Token::Question);
                        parser.expect(&Token::Colon)?;
                        record.fields.push(RecordTypeField {
                            name,
                            ty: parser.ty()?,
                            optional,
                        });
                        if !parser.eat(&Token::Comma) {
                            parser.expect(&Token::RBrace)?;
                            break;
                        }
                    }
                    Type::Record(record)
                }
                _ => return Err(parser.unexpected_previous("a type")),
            })
        })
    }

    fn formula(&mut self) -> Parsed<Formula> {
        self.nested(|parser| {
            if let Token::Int(amount) = parser.peek() {
                let amount = u64::try_from(amount.as_bigint())
                    .map_err(|_| parser.unexpected("an amount that fits 64 bits"))?;
                parser.position += 1;
                return Ok(Formula::Constant(amount));
            }
            let word = parser.keyword("a formula")?;
            parser.expect(&Token::LParen)?;
            let formula = match word.as_str() {
                "size" => Formula::Size(parser.operand()?),
                "deep" => Formula::DeepSize(parser.operand()?),
                "magnitude" => Formula::Magnitude(parser.operand()?),
                "sum" | "product" | "max" | "min" => {
                    let mut terms = Vec::new();
                    while parser.peek() != &Token::RParen {
                        terms.push(parser.formula()?);
                        if !parser.eat(&Token::Comma) {
                            break;
                        }
                    }
                    match word.as_str() {
                        "sum" => Formula::Sum(terms),
                        "product" => Formula::Product(terms),
                        "max" => Formula::Max(terms),
                        _ => Formula::Min(terms),
                    }
                }
                _ => return Err(parser.unexpected_previous("a formula")),
            };
            parser.expect(&Token::RParen)?;
            Ok(formula)
        })
    }

    fn operand(&mut self) -> Parsed<Operand> {
        if self.eat_word("result") {
            Ok(Operand::Result)
        } else {
            Ok(Operand::Param(self.name()?))
        }
    }
}
