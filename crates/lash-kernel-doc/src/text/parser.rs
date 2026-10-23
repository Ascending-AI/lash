//! Reads kernel text.

mod types;

use std::collections::{BTreeMap, BTreeSet};

use num_traits::ToPrimitive;

use super::lexer::{Spanned, Token, lex};
use super::{ParseError, ParseErrorReason};

use crate::FunctionTarget;
use crate::ast::{
    ActExpr, ActKind, Action, AssignOrder, Atom, Block, Callee, Catch, Closure, Expr, JoinMode,
    Literal, MapEntry, Member, Place, ProjectionRead, Protocol, RecordEntry, Rhs, Stmt, TryStmt,
};

use crate::document::Document;
use crate::function::{Formula, FunctionBody, FunctionDefinition, Guard, Implementation, Operand};

use crate::function_value::{CallablePresentation, FunctionRepresentation, StartConvention};

use crate::number::{Float, NumberPolicy};
use lash_vocabulary::names::{FunctionId, FunctionName, FunctionName as QualifiedName, Name};

use crate::validate::StatementForm;
use crate::value::Bytes;
use lash_vocabulary::names::{
    Brand, Convention, Import, ImportKind, ImportSlot, PackagePath, QualifiedName as ImportName,
    Segment,
};
use lash_vocabulary::type_text::is_keyword;
use lash_vocabulary::types::{Param, Signature, Type};

/// Reads a document from kernel text.
///
/// A document is one whole workflow: the `kernel` and `numbers` header
/// lines, an `entry` signature for each declared function a host may start,
/// and `main`. [`print_document`] writes the canonical text that parses
/// back to it. Statements end at the line; a `;` is a refusal that names
/// its line and column:
///
/// ```rust
#[doc = include_str!("document_example.rs")]
/// ```
///
/// The example is `text/document_example.rs`, run by
/// `tests/document_example.rs`; the workspace builds no doctests, so that
/// test is its proof.
pub fn parse_document(text: &str) -> Result<Document, ParseError> {
    parse_document_with_limits(
        text,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
pub fn parse_document_with_limits(
    text: &str,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<Document, ParseError> {
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    document.main = None;
    parse_document_over(text, document, limits)
}

/// Reads text beside an already typed prelude. Internal document assembly
/// retains definitions and Imports as facts instead of printing their bodies
/// only to parse them back. Text items keep the ordinary duplicate checks.
pub fn parse_document_over(
    text: &str,
    mut document: Document,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<Document, ParseError> {
    let mut parser = Parser::new(text, limits)?;
    while parser.peek() != &Token::End {
        match parser.keyword("a document item")?.as_str() {
            "kernel" => {
                let kernel = parser.version()?;
                document.requires.kernels = std::collections::BTreeSet::from([kernel]);
            }
            "numbers" => {
                document.numbers = match parser.keyword("number policy")?.as_str() {
                    "float" => NumberPolicy::Float,
                    "by_spelling" => NumberPolicy::BySpelling,
                    _ => return Err(parser.unexpected_previous("number policy")),
                };
            }
            "import" => parser.import_line(&mut document.imports)?,
            "export" => {
                let name = parser.name()?;
                let text = parser.text()?;
                let export = serde_json::from_str(&text)
                    .map_err(|error| parser.malformed(error.to_string()))?;
                parser.once(document.exports.insert(name, export), "Export")?;
            }
            "definition" => {
                let definition = parse_definition_with_limits(&parser.text()?, limits)?;
                let id = definition.identity();
                parser.once(
                    document.functions.insert(id, definition),
                    "function identity",
                )?;
            }
            "library" => {
                document.library.insert(parser.function_reference()?);
            }
            "native" => {
                document
                    .requires
                    .natives
                    .insert(parser.function_reference()?);
            }
            "private" => loop {
                document.private_bindings.insert(parser.name()?);
                if !parser.eat(&Token::Comma) {
                    break;
                }
            },
            "main" => {
                parser.body_imports = document.imports.clone();
                parser.uses = document.function_names();
                let block = parser.block()?;
                parser.once(document.main.replace(block), "main")?;
            }
            _ => return Err(parser.unexpected_previous("a document item")),
        }
    }
    document
        .functions
        .extend(parser.locals.into_iter().map(|f| (f.identity(), f)));
    Ok(document)
}

/// Reads a signature, `(name: Type, …) -> Type`, from kernel text: the
/// notation a package surface declares its members' slots in.
pub fn parse_signature(text: &str) -> Result<Signature, ParseError> {
    let mut parser = Parser::new(
        text,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )?;
    let signature = parser.signature()?;
    if parser.peek() != &Token::End {
        return Err(parser.unexpected("the end of the signature"));
    }
    Ok(signature)
}

/// Reads a library-function definition from kernel text.
pub fn parse_definition(text: &str) -> Result<FunctionDefinition, ParseError> {
    parse_definition_with_limits(
        text,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
pub fn parse_definition_with_limits(
    text: &str,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<FunctionDefinition, ParseError> {
    let (definition, locals) = parse_definition_with_locals_and_limits(text, limits)?;
    if !locals.is_empty() {
        return Err(ParseError {line:1,column:1,reason:ParseErrorReason::Malformed {problem:"inline closures require parse_definition_with_locals so their definitions are retained".into()}});
    }
    Ok(definition)
}
/// Parse a helper and lift each inline closure into its own local function definition.
pub fn parse_definition_with_locals(
    text: &str,
) -> Result<(FunctionDefinition, Vec<FunctionDefinition>), ParseError> {
    parse_definition_with_locals_and_limits(
        text,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
}
pub fn parse_definition_with_locals_and_limits(
    text: &str,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<(FunctionDefinition, Vec<FunctionDefinition>), ParseError> {
    parse_definition_over(text, BTreeMap::new(), limits)
}

/// Read a helper against owner-declared operation imports. Names resolve only
/// at perform syntax; literal tokens never participate in operation resolution.
pub fn parse_definition_over(
    text: &str,
    imports: BTreeMap<ImportSlot, Import>,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<(FunctionDefinition, Vec<FunctionDefinition>), ParseError> {
    crate::document_work::note_definition_parse();
    let mut parser = Parser::new(text, limits)?;
    parser.body_imports = imports.clone();
    parser.expect_keyword("function")?;
    let name = parser.qualified_name()?;
    let signature = parser.signature()?;
    let mut kernel = None;
    let mut errors = BTreeSet::new();
    let mut charge = None;
    let mut statements = false;
    let mut guard = None;
    let mut native = false;
    let mut native_version = None;
    let prelude_imports = imports.len();
    let mut imports = imports;
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
            "metering" => {
                parser.expect_keyword("statements")?;
                statements = true;
            }
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
            "import" => parser.import_line(&mut imports)?,
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
            parser.body_imports = imports.clone();
            parser.kernel = kernel.unwrap_or(crate::KERNEL_VERSION);
            let block = parser.block()?;
            for local in &parser.locals {
                parser.uses.insert(local.identity(), local.name.clone());
            }
            Some(FunctionBody {
                functions: std::mem::take(&mut parser.uses),
                imports,
                block,
            })
        }
        // Only a body performs: an `effect` line needs one.
        None if imports.len() > prelude_imports => {
            return Err(parser.missing("the `body` its `effect` lines are performed by"));
        }
        None => None,
    };
    let implementation = match (native, body) {
        (true, None) => Implementation::Native {
            charge: charge.ok_or_else(|| parser.missing("native charge formula"))?,
        },
        (false, Some(body)) => Implementation::Guest {
            body,
            metering: if statements {
                crate::Metering::Statements
            } else {
                crate::Metering::Formula(charge.ok_or_else(|| parser.missing("guest metering"))?)
            },
        },
        (true, Some(body)) => Implementation::Both {
            body,
            charge: charge.ok_or_else(|| parser.missing("native charge formula"))?,
        },
        (false, None) => return Err(parser.missing("native or body")),
    };
    let definition = FunctionDefinition {
        kernel: kernel.ok_or_else(|| parser.missing("the `kernel` line"))?,
        name,
        signature,
        errors,
        guard,
        implementation,
        native_version: native_version.unwrap_or(crate::FIRST_NATIVE_VERSION),
    };
    Ok((definition, parser.locals))
}

struct Parser {
    tokens: Vec<Spanned>,
    position: usize,
    /// The `use` lines read so far.
    uses: BTreeMap<FunctionId, FunctionName>,
    body_imports: BTreeMap<ImportSlot, Import>,
    kernel: u32,
    locals: Vec<FunctionDefinition>,
    depth: usize,
    limit: usize,
}

type Parsed<T> = Result<T, ParseError>;

impl Parser {
    fn new(text: &str, limits: lash_vocabulary::data_limits::KernelDataLimits) -> Parsed<Self> {
        Ok(Self {
            tokens: lex(text)?,
            position: 0,
            uses: BTreeMap::new(),
            body_imports: BTreeMap::new(),
            kernel: crate::KERNEL_VERSION,
            locals: Vec::new(),
            depth: 0,
            limit: limits.document_depth(),
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
            Token::Int(version) => version
                .to_u32()
                .ok_or_else(|| self.unexpected_previous("a kernel version")),
            _ => Err(self.unexpected_previous("a kernel version")),
        }
    }

    fn malformed(&self, problem: String) -> ParseError {
        let mut error = self.unexpected("valid document data");
        error.reason = ParseErrorReason::Malformed { problem };
        error
    }
    fn operation_slot(&mut self) -> Parsed<ImportSlot> {
        if matches!(self.peek(), Token::Int(_)) {
            return Ok(ImportSlot(self.version()?));
        }
        let start = self.position;
        let spelling = self.qualified_name()?.to_string();
        let operation = lash_vocabulary::operation_text::parse(&spelling)
            .map_err(|error| self.malformed(error.to_string()))?;
        let name = ImportName {
            package: operation.package,
            member: operation.name,
            type_member: None,
        };
        let mut matches = self.body_imports.iter().filter(|(_, import)| {
            matches!(import.kind, ImportKind::Operation { .. }) && import.name == name
        });
        let slot = matches.next().map(|(slot, _)| *slot).ok_or_else(|| {
            self.error_at(start, ParseErrorReason::UnknownOperation { name: spelling })
        })?;
        if matches.next().is_some() {
            return Err(self.error_at(
                start,
                ParseErrorReason::Duplicate {
                    item: format!("operation {name}"),
                },
            ));
        }
        Ok(slot)
    }

    fn import_line(&mut self, imports: &mut BTreeMap<ImportSlot, Import>) -> Parsed<()> {
        let slot = ImportSlot(self.version()?);
        let kind = self.keyword("Import kind")?;
        let text = self.qualified_name()?.to_string();
        let mut segments = text.split('.').collect::<Vec<_>>();
        let member = Segment::new(segments.pop().unwrap_or_default())
            .map_err(|error| self.malformed(error.to_string()))?;
        let package = PackagePath::parse(&segments.join("."))
            .map_err(|error| self.malformed(error.to_string()))?;
        let name = ImportName {
            package: package.clone(),
            member: member.clone(),
            type_member: None,
        };
        let kind = match kind.as_str() {
            "operation" => ImportKind::Operation {
                signature: self.signature()?,
            },
            "code" => {
                let signature = self.signature()?;
                let convention = match self.keyword("semantic convention")?.as_str() {
                    "direct" => Convention::Direct,
                    "ts_direct" => Convention::TsDirect,
                    "ts_args" => Convention::TsArgsList,
                    "py_direct" => Convention::PyDirect,
                    _ => return Err(self.unexpected_previous("semantic convention")),
                };
                ImportKind::Code {
                    signature,
                    convention,
                }
            }
            "type" => ImportKind::Type {
                brand: Brand {
                    package,
                    name: member,
                },
            },
            "error" => ImportKind::Error {
                kind: Brand {
                    package,
                    name: member,
                },
            },
            _ => return Err(self.unexpected_previous("Import kind")),
        };
        self.once(imports.insert(slot, Import { name, kind }), "Import slot")
    }
    fn target(&mut self) -> Parsed<FunctionTarget> {
        if self.eat_word("import") {
            Ok(FunctionTarget::Import(ImportSlot(self.version()?)))
        } else {
            self.function_reference().map(FunctionTarget::Local)
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
        if self.depth >= self.limit {
            return Err(self.error(ParseErrorReason::TooDeep { limit: self.limit }));
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
                let value = self.rhs()?;
                // `place first`: the place before the right-hand side, which
                // is then an expression (`K-EVAL-003`).
                if self.eat_word("place") {
                    self.expect_keyword("first")?;
                    let value = match value {
                        Rhs::Action(action) => Rhs::Expr(action.into()),
                        value => value,
                    };
                    Stmt::Assign {
                        place,
                        value,
                        order: AssignOrder::PlaceFirst,
                    }
                } else {
                    Stmt::Assign {
                        place,
                        value,
                        order: AssignOrder::ValueFirst,
                    }
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
            "do" => Stmt::Do {
                value: self.expr()?,
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
                let iterable = self.expr()?;
                let via = if self.eat_word("via") {
                    Some(self.protocol()?)
                } else {
                    None
                };
                let metered = self.loop_meter()?;
                let body = self.block()?;
                Stmt::For {
                    metered,
                    binding,
                    iterable,
                    via,
                    body,
                    else_block: self.loop_else()?,
                }
            }
            "while" => {
                let condition = self.expr()?;
                let metered = self.loop_meter()?;
                Stmt::While {
                    metered,
                    condition,
                    body: self.block()?,
                    else_block: self.loop_else()?,
                }
            }
            "break" => Stmt::Break,
            "continue" => Stmt::Continue,
            "return" => {
                let value = self.expr()?;
                if self.eat_word("via") {
                    Stmt::ReturnVia {
                        value,
                        policy: self.target()?,
                    }
                } else {
                    Stmt::Return { value }
                }
            }
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

    /// A right-hand side. An action that may nest is read as an
    /// expression; when it is the whole right-hand side and every argument
    /// is a variable or a literal, it is the statement's action.
    fn rhs(&mut self) -> Parsed<Rhs> {
        if !self.at_named_call()
            && matches!(
                self.statement_form(),
                Some(StatementForm::Yield | StatementForm::Cancel)
            )
            && let Some(action) = self.action()?
        {
            return Ok(Rhs::Action(action));
        }
        Ok(match self.expr()? {
            Expr::Act(act) => match act.as_action() {
                Some(action) => Rhs::Action(action),
                None => Rhs::Expr(Expr::Act(act)),
            },
            expr => Rhs::Expr(expr),
        })
    }

    /// An action nested in an expression, when the next word begins one:
    /// `call`, `apply` or `invoke` a function, `perform`, `sleep`, `spawn`,
    /// `join` or `await`. `yield` and `cancel` have no expression spelling.
    fn act(&mut self) -> Parsed<Option<ActExpr>> {
        let kind = match self.statement_form() {
            Some(StatementForm::Call) => ActKind::Call(self.act_callee()?),
            Some(StatementForm::Spawn) => {
                self.position += 1;
                ActKind::Spawn(self.act_callee()?)
            }
            Some(StatementForm::Perform) => {
                self.position += 1;
                let slot = self.operation_slot()?;
                self.expect(&Token::LParen)?;
                let args = self.exprs(&Token::RParen)?;
                self.expect_keyword("as")?;
                let result = self.ty()?;
                let then_yield = self.then_yield()?;
                return Ok(Some(ActExpr {
                    kind: ActKind::Perform {
                        slot,
                        result,
                        then_yield,
                    },
                    args,
                }));
            }
            Some(StatementForm::Join) => {
                self.position += 1;
                let kind = match self.join_mode() {
                    Some(mode) => ActKind::JoinMany(mode),
                    None => ActKind::Join,
                };
                return Ok(Some(ActExpr {
                    kind,
                    args: vec![self.act_operand()?],
                }));
            }
            Some(StatementForm::Yield) => {
                self.position += 1;
                return Ok(Some(ActExpr {
                    kind: ActKind::Yield,
                    args: vec![],
                }));
            }
            Some(StatementForm::Cancel) => {
                self.position += 1;
                return Ok(Some(ActExpr {
                    kind: ActKind::Cancel,
                    args: vec![self.act_operand()?],
                }));
            }
            Some(StatementForm::Await) => {
                self.position += 1;
                let value = self.act_operand()?;
                self.expect_keyword("via")?;
                return Ok(Some(ActExpr {
                    kind: ActKind::Await(self.target()?),
                    args: vec![value],
                }));
            }
            _ => return Ok(None),
        };
        self.expect(&Token::LParen)?;
        Ok(Some(ActExpr {
            kind,
            args: self.exprs(&Token::RParen)?,
        }))
    }

    /// The one argument of a nested `sleep`, `join` or `await`: `(e)`
    /// nests any expression; a bare variable or literal is the statement's
    /// spelling.
    fn act_operand(&mut self) -> Parsed<Expr> {
        if self.eat(&Token::LParen) {
            let operand = self.expr()?;
            self.expect(&Token::RParen)?;
            return Ok(operand);
        }
        Ok(match self.atom()? {
            Atom::Variable(name) => Expr::Variable(name),
            Atom::Literal(literal) => Expr::Literal(literal),
        })
    }

    /// A `then yield` after a `perform` or a `sleep`.
    fn then_yield(&mut self) -> Parsed<bool> {
        if !self.is_word("then") {
            return Ok(false);
        }
        self.position += 1;
        self.expect_keyword("yield")?;
        Ok(true)
    }

    /// The mode of a list `join`, when the next word names one.
    fn join_mode(&mut self) -> Option<JoinMode> {
        let Token::Word(word) = self.peek() else {
            return None;
        };
        let mode = match word.as_str() {
            "all" => JoinMode::All,
            "settled" => JoinMode::AllSettled,
            "race" => JoinMode::Race,
            "any" => JoinMode::Any,
            _ => return None,
        };
        self.position += 1;
        Some(mode)
    }

    /// What a nested call or `spawn` runs: `call`, `apply` or `invoke` and
    /// its function.
    fn act_callee(&mut self) -> Parsed<Callee> {
        Ok(
            match self.keyword("`call`, `apply` or `invoke`")?.as_str() {
                "call" => self.target()?.callee(),
                "apply" => Callee::Value(self.name()?),
                "invoke" => self.target()?.callee(),
                _ => return Err(self.unexpected_previous("`call`, `apply` or `invoke`")),
            },
        )
    }

    /// The statement form the next word begins, if it begins one.
    fn statement_form(&self) -> Option<StatementForm> {
        let Token::Word(word) = self.peek() else {
            return None;
        };
        Some(match word.as_str() {
            "call" | "apply" | "invoke" => StatementForm::Call,
            "perform" => StatementForm::Perform,
            "join" => StatementForm::Join,
            "yield" => StatementForm::Yield,
            "spawn" => StatementForm::Spawn,
            "cancel" => StatementForm::Cancel,
            "await" => StatementForm::Await,
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
                let slot = self.operation_slot()?;
                let args = self.atoms()?;
                self.expect_keyword("as")?;
                Action::Perform {
                    slot,
                    args,
                    result: self.ty()?,
                    then_yield: self.then_yield()?,
                }
            }
            StatementForm::Join => {
                self.position += 1;
                match self.join_mode() {
                    Some(mode) => Action::JoinMany {
                        mode,
                        tasks: self.atom()?,
                    },
                    None => Action::Join { task: self.atom()? },
                }
            }
            StatementForm::Await => {
                self.position += 1;
                let value = self.atom()?;
                self.expect_keyword("via")?;
                Action::Await {
                    policy: self.target()?,
                    value,
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
        let callee = self.act_callee()?;
        Ok((callee, self.atoms()?))
    }

    /// A library function: `@<hex>`, or the name one `use` line gives.
    /// `(open f, next f, item f, check f)`: a loop protocol, its hooks in
    /// this order, each but `open` optional.
    fn protocol(&mut self) -> Parsed<Protocol> {
        self.expect(&Token::LParen)?;
        self.expect_keyword("open")?;
        let open = self.target()?;
        let mut hooks = [None, None, None];
        let mut after = 0;
        while self.eat(&Token::Comma) {
            let Some(at) = ["next", "item", "check"]
                .iter()
                .skip(after)
                .position(|word| self.is_word(word))
            else {
                return Err(self.unexpected("`next`, `item` or `check`, in that order"));
            };
            self.position += 1;
            hooks[after + at] = Some(self.target()?);
            after += at + 1;
        }
        self.expect(&Token::RParen)?;
        let [next, item, check] = hooks;
        Ok(Protocol {
            open,
            next,
            item,
            check,
        })
    }

    /// A loop's `else` block, or none.
    fn loop_else(&mut self) -> Parsed<Block> {
        if self.eat_word("else") {
            self.block()
        } else {
            Ok(Vec::new())
        }
    }

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
                return Ok(Some(Literal::Function(self.function_reference()?)));
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
        let reserved = single
            && (self.is_word("fn")
                || self.is_word("read")
                || self.is_word("if")
                || self.is_word("let")
                || self.is_word("sleep")
                || self.is_word("join")
                || self.is_word("await")
                || self.is_word("yield")
                || self.is_word("cancel"));
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
                if parser.depth + links > parser.limit {
                    return Err(parser.error(ParseErrorReason::TooDeep {
                        limit: parser.limit,
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
        if let Some(act) = self.act()? {
            return Ok(Expr::Act(Box::new(act)));
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
                "if" => {
                    self.expect(&Token::LParen)?;
                    let cond = Box::new(self.expr()?);
                    self.expect(&Token::Comma)?;
                    let then = Box::new(self.expr()?);
                    self.expect(&Token::Comma)?;
                    let r#else = Box::new(self.expr()?);
                    self.expect(&Token::RParen)?;
                    Ok(Expr::If { cond, then, r#else })
                }
                "let" => {
                    self.expect(&Token::LParen)?;
                    let name = self.name()?;
                    self.expect(&Token::Equals)?;
                    let value = Box::new(self.expr()?);
                    self.expect(&Token::Comma)?;
                    let body = Box::new(self.expr()?);
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Let { name, value, body })
                }
                "read" => {
                    self.expect(&Token::LParen)?;
                    let handle = self.expr()?;
                    self.expect(&Token::Comma)?;
                    let request = self.expr()?;
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Read(Box::new(ProjectionRead { handle, request })))
                }
                "fn" => {
                    let (function, start) = if self.peek() == &Token::LParen {
                        let params = self.names()?;
                        let start = self.start()?;
                        let block = self.block()?;
                        let held = crate::Node::Block(&block).functions_within();
                        let performed = crate::Node::Block(&block).imports_within();
                        let body = FunctionBody {
                            functions: self
                                .uses
                                .iter()
                                .filter(|(id, _)| held.contains(id))
                                .map(|(id, name)| (*id, name.clone()))
                                .collect(),
                            imports: self
                                .body_imports
                                .iter()
                                .filter(|(slot, _)| performed.contains(slot))
                                .map(|(slot, import)| (*slot, import.clone()))
                                .collect(),
                            block,
                        };
                        let definition = FunctionDefinition {
                            kernel: self.kernel,
                            name: FunctionName::new("kernel.anonymous")
                                .unwrap_or_else(|_| unreachable!("valid name")),
                            signature: Signature {
                                params: params
                                    .into_iter()
                                    .map(|name| Param {
                                        name,
                                        ty: Type::Any,
                                        optional: false,
                                    })
                                    .collect(),
                                result: Type::Any,
                            },
                            errors: BTreeSet::new(),
                            guard: None,
                            implementation: Implementation::Guest {
                                body,
                                metering: crate::Metering::Statements,
                            },
                            native_version: crate::FIRST_NATIVE_VERSION,
                        };
                        let id = definition.identity();
                        self.uses.insert(id, definition.name.clone());
                        self.locals.push(definition);
                        (id, start)
                    } else {
                        (self.function_reference()?, self.start()?)
                    };
                    Ok(Expr::Closure(Box::new(Closure {
                        function,
                        presentation: start,
                    })))
                }
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

    /// A start adapter, if one follows a function's parameters:
    /// `start <signature> bare|token(<tag>, <slot>) direct|ts_args(<await>, <same>)`.
    fn start(&mut self) -> Parsed<Option<CallablePresentation>> {
        if !self.is_word("presentation") {
            return Ok(None);
        }
        self.position += 1;
        let signature = self.signature()?;
        let representation = match self.keyword("`bare` or `token`")?.as_str() {
            "bare" => FunctionRepresentation::Bare,
            "token" => {
                self.expect(&Token::LParen)?;
                let Token::Text(tag) = self.advance() else {
                    return Err(self.unexpected_previous("a text tag"));
                };
                self.expect(&Token::Comma)?;
                let callable = self.version()?;
                self.expect(&Token::RParen)?;
                FunctionRepresentation::Token { tag, callable }
            }
            _ => return Err(self.unexpected_previous("`bare` or `token`")),
        };
        let convention = self.convention()?;
        Ok(Some(CallablePresentation {
            signature,
            representation,
            convention,
        }))
    }

    fn convention(&mut self) -> Parsed<StartConvention> {
        let expected = "`direct`, `ts_direct`, `ts_args` or `py_direct`";
        let convention = match self.keyword(expected)?.as_str() {
            "direct" => StartConvention::Direct,
            "ts_direct" => {
                self.expect(&Token::LParen)?;
                let await_fn = self.target()?;
                self.expect(&Token::RParen)?;
                StartConvention::TsDirect { await_fn }
            }
            "ts_args" => {
                self.expect(&Token::LParen)?;
                let await_fn = self.target()?;
                self.expect(&Token::Comma)?;
                let same_fn = self.target()?;
                self.expect(&Token::RParen)?;
                StartConvention::TsArgsList { await_fn, same_fn }
            }
            "py_direct" => {
                self.expect(&Token::LParen)?;
                let cleanup = self.target()?;
                self.expect(&Token::RParen)?;
                StartConvention::PyDirect { cleanup }
            }
            _ => return Err(self.unexpected_previous(expected)),
        };
        Ok(convention)
    }

    fn loop_meter(&mut self) -> Parsed<u64> {
        if !self.eat_word("metered") {
            return Ok(0);
        }
        let Token::Int(amount) = self.peek() else {
            return Err(self.unexpected("a positive loop charge that fits 64 bits"));
        };
        let amount = amount
            .to_u64()
            .filter(|amount| *amount > 0)
            .ok_or_else(|| self.unexpected("a positive loop charge that fits 64 bits"))?;
        self.position += 1;
        Ok(amount)
    }

    fn formula(&mut self) -> Parsed<Formula> {
        self.nested(|parser| {
            if let Token::Int(amount) = parser.peek() {
                let amount = amount
                    .to_u64()
                    .ok_or_else(|| parser.unexpected("an amount that fits 64 bits"))?;
                parser.position += 1;
                return Ok(Formula::Constant(amount));
            }
            let word = parser.keyword("a formula")?;
            parser.expect(&Token::LParen)?;
            let formula = match word.as_str() {
                "size" => Formula::Size(parser.operand()?),
                "deep" => Formula::DeepSize(parser.operand()?),
                "nested" => Formula::NestedSize(parser.operand()?),
                "magnitude" => Formula::Magnitude(parser.operand()?),
                "ceil_log2" => Formula::CeilLog2(Box::new(parser.formula()?)),
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
