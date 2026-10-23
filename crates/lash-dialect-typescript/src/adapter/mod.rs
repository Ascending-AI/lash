use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crate::captions::{SourceComments, Spelled};
use crate::{Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};
use swc_common::Spanned;
use swc_ecma_ast as swc;

mod assignments;
mod captions;
mod declarations;
mod early_errors;
pub(crate) use early_errors::is_strict_reserved_word;
mod enums;
mod functions;
pub(crate) use functions::public_pattern;
mod goal;
pub(crate) mod nesting;
mod optional_chain;
mod parser;
mod workflow;
pub(crate) use workflow::parse as parse_workflow;
mod repairs;
pub(crate) use parser::Parser;
mod prototype_chain;
mod rejections;
mod template;
mod types;
use enums::{ConstEnumValue, enum_member_property_name};
use goal::Goal;
use nesting::{guard_source_nesting, source_nesting_diagnostic};
use prototype_chain::{check_property_key, is_prototype_chain_property};
use rejections::{parser_diagnostic, reject, reject_defect, reject_refusal, source_span};
use types::unasserted;
pub(crate) use types::{TypeAnnotation, TypeShape};

/// This is deliberately below the shared AST and 2 MiB native-stack limits.
pub const MAX_SOURCE_NESTING_DEPTH: usize = 28;

#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub(crate) statements: Vec<Stmt>,
    pub(crate) imported: BTreeMap<String, Vec<String>>,
    pub(crate) annotation_limits: (usize, usize),
    /// The captions the source spells.
    pub(crate) captions: Spelled,
    /// Each comment's bytes, ascending; a cell's are not collected.
    pub(crate) comments: Vec<std::ops::Range<usize>>,
}

#[derive(Clone, Debug)]
pub(crate) enum Stmt {
    Spanned(SourceSpan, Box<Stmt>),
    Empty,
    Expr(Expr),
    /// `type Name = ...` or `interface Name { ... }`: it runs nothing, and
    /// the type analysis reads what `Name` stands for.
    TypeAlias {
        name: String,
        ty: TypeAnnotation,
    },
    Block(Vec<Stmt>),
    Var {
        kind: VarKind,
        declarations: Vec<Var>,
    },
    Enum {
        name: String,
        members: Vec<EnumMember>,
    },
    Function {
        name: String,
        function: Function,
    },
    Return(Option<Expr>),
    If {
        test: Expr,
        consequent: Box<Stmt>,
        alternate: Option<Box<Stmt>>,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        test: Expr,
        #[expect(dead_code, reason = "kept for the diagnostics of the loop's condition")]
        test_span: SourceSpan,
    },
    For {
        init: Option<Box<Stmt>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Stmt>,
    },
    ForOf {
        pattern: Pattern,
        kind: Option<VarKind>,
        iterable: Expr,
        body: Box<Stmt>,
    },
    ForIn {
        pattern: Pattern,
        kind: Option<VarKind>,
        object: Expr,
        body: Box<Stmt>,
    },
    Switch {
        discriminant: Expr,
        cases: Vec<SwitchCase>,
    },
    Break,
    Continue,
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        catch: Option<Catch>,
        finally: Option<Vec<Stmt>>,
    },
}

impl Stmt {
    /// The statement itself, with its source span peeled off.
    ///
    /// Every pass that asks what kind of statement this is — hoisting,
    /// pre-declaration, loop analysis — asks through here.
    pub(crate) fn unspanned(&self) -> &Self {
        match self {
            Self::Spanned(_, stmt) => stmt.unspanned(),
            other => other,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EnumMember {
    pub(crate) name: String,
    pub(crate) value: Expr,
    pub(crate) reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Clone, Debug)]
pub(crate) struct Var {
    pub(crate) pattern: Pattern,
    pub(crate) init: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct Catch {
    pub(crate) binding: Option<Pattern>,
    pub(crate) body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub(crate) struct SwitchCase {
    pub(crate) test: Option<Expr>,
    pub(crate) consequent: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    /// A bound name, with the type annotation written on it if there was
    /// one. An optional parameter's annotation is its type or `undefined`.
    Ident(String, Option<TypeAnnotation>),
    Rest(Box<Pattern>),
    Member {
        object: Box<Expr>,
        property: MemberProperty,
    },
    Assign {
        target: Box<Pattern>,
        default: Box<Expr>,
    },
    Array {
        elements: Vec<Option<Pattern>>,
        rest: Option<Box<Pattern>>,
    },
    Object {
        properties: Vec<ObjectPatternProperty>,
        rest: Option<Box<Pattern>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ObjectPatternProperty {
    pub(crate) key: PropertyKey,
    pub(crate) value: Pattern,
}

#[derive(Clone, Debug)]
pub(crate) enum PropertyKey {
    Static(String),
    Computed(Box<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) struct Function {
    pub(crate) name: Option<String>,
    pub(crate) params: Vec<Pattern>,
    pub(crate) param_spans: Vec<SourceSpan>,
    pub(crate) result_span: SourceSpan,
    pub(crate) type_parameters: Vec<String>,
    pub(crate) body: FunctionBody,
    pub(crate) return_ty: Option<TypeAnnotation>,
    pub(crate) is_async: bool,
    /// An arrow has no receiver of its own: its `this` is its enclosing
    /// function's (ECMA-262 lexical `this`), and its `arguments` binds the
    /// enclosing non-arrow function's arguments object, not one of its own.
    pub(crate) is_arrow: bool,
    /// What a workflow module's source captions of its parameters and
    /// their and its result's types; nothing for a cell or an arrow.
    pub(crate) captions: crate::captions::SignatureCaptions,
}

#[derive(Clone, Debug)]
pub(crate) enum FunctionBody {
    Block(Vec<Stmt>),
    Expression(Box<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) enum Expr {
    Null,
    Bool(bool),
    Number(f64),
    BigInt(lash_kernel_doc::Integer),
    String(String),
    RegExp {
        pattern: String,
        flags: String,
    },
    /// A bare identifier reference. The span is the identifier's own extent in
    /// the submitted source; it is `None` for identifiers the adapter or the
    /// lowerer synthesises, which have no source text of their own.
    Ident(String, Option<SourceSpan>),
    This,
    Array(Vec<ArrayElement>),
    Object(Vec<ObjectProperty>),
    Assign {
        target: AssignTarget,
        op: AssignOp,
        value: Box<Expr>,
    },
    Member {
        object: Box<Expr>,
        property: MemberProperty,
        span: SourceSpan,
    },
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        /// Source operands for repairs; synthesized binary tests have none.
        operand_spans: Option<[SourceSpan; 2]>,
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Logical {
        left: Box<Expr>,
        op: LogicalOp,
        right: Box<Expr>,
    },
    Conditional {
        test: Box<Expr>,
        consequent: Box<Expr>,
        alternate: Box<Expr>,
    },
    Template {
        quasis: Vec<String>,
        expressions: Vec<(SourceSpan, Expr)>,
    },
    Function(Function),
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
        span: SourceSpan,
    },
    New {
        constructor: String,
        args: Vec<CallArg>,
    },
    OptionalChain {
        base: Box<Expr>,
        operations: Vec<OptionalOperation>,
    },
    Await {
        value: Box<Expr>,
        span: SourceSpan,
    },
    Update {
        target: AssignTarget,
        delta: f64,
        prefix: bool,
    },
    Delete {
        object: Box<Expr>,
        property: MemberProperty,
    },
    /// `value as T` and `<T>value`: the value itself, which the type
    /// analysis believes to be a `T`.
    As {
        value: Box<Expr>,
        ty: TypeAnnotation,
    },
    LoneSurrogateString,
}

#[derive(Clone, Debug)]
pub(crate) enum ArrayElement {
    Value(Expr),
    Spread(Expr),
    /// An elision — `[1, , 3]` — ECMA's hole: the slot has no own property at
    /// all, distinct from a stored `undefined`.
    Hole,
}

#[derive(Clone, Debug)]
pub(crate) enum ObjectProperty {
    KeyValue(PropertyKey, Expr),
    Spread(Expr),
}

#[derive(Clone, Debug)]
pub(crate) enum CallArg {
    Value(Expr),
    Spread(Expr),
}

#[derive(Clone, Debug)]
pub(crate) enum OptionalOperation {
    Member {
        property: MemberProperty,
        optional: bool,
    },
    Call {
        args: Vec<CallArg>,
        optional: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum AssignTarget {
    Ident(String),
    /// A parenthesized identifier target — `(x) = 1`. ECMA-262 covers the
    /// parentheses away, so it assigns `x`; but a covered name is not an
    /// IdentifierReference, so an anonymous function assigned to it gets no
    /// NamedEvaluation name.
    ParenIdent(String),
    Member {
        object: Box<Expr>,
        property: MemberProperty,
    },
    Pattern(Box<Pattern>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AssignOp {
    Assign,
    Binary(BinaryOp),
    Logical(LogicalOp),
}

#[derive(Clone, Debug)]
pub(crate) enum MemberProperty {
    Field(String),
    Index(Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Plus,
    Minus,
    Not,
    TypeOf,
    Void,
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Exponent,
    BitAnd,
    BitOr,
    BitXor,
    ShiftLeft,
    ShiftRight,
    ShiftRightUnsigned,
    In,
    InstanceOf,
    StrictEqual,
    StrictNotEqual,
    LooseEqual,
    LooseNotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LogicalOp {
    And,
    Or,
    Nullish,
}

/// The standard TypeScript cell admission budget. Sources above it reject
/// with `TS_SOURCE_TOO_LARGE`; the bound is what makes the parse stack
/// reservation below finite. 64 KiB is roughly 1 600 lines of TypeScript.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;

mod source_limits;
pub use source_limits::SourceLimits;

mod parser_stack;
pub use parser_stack::ParserStack;
use parser_stack::parse_on_proportional_stack;

fn guard_source_size(source: &str, limits: SourceLimits) -> Result<(), Diagnostic> {
    let max_bytes = limits.max_bytes();
    if source.len() > max_bytes {
        return Err(Diagnostic::with_repair(
            DiagnosticCode::SourceTooLarge,
            format!(
                "TypeScript source is {} bytes, over the configured {max_bytes}-byte source limit",
                source.len()
            ),
            "split the work across several cells",
            None,
        ));
    }
    Ok(())
}

/// The full front-end entry: bound the source, reject nesting with a
/// source-level diagnostic, then parse on a stack sized for the source.
pub(crate) fn parse(source: &str) -> Result<Program, Diagnostic> {
    parse_with_limits(source, SourceLimits::standard())
}
pub(crate) fn parse_with_limits(source: &str, limits: SourceLimits) -> Result<Program, Diagnostic> {
    guard_source_size(source, limits)?;
    guard_source_nesting(source, limits.max_nesting())?;
    parse_on_proportional_stack(source, false, limits)
}

fn parse_source(source: &str, limits: SourceLimits) -> Result<Program, Diagnostic> {
    parse_from_goal(source, Goal::Module, false, limits)
}

/// Reads `source` under the first goal, from `first` on, that admits it (see
/// [`Goal`]), converts it, and checks the early errors of its declarations.
fn parse_from_goal(
    source: &str,
    first: Goal,
    workflow: bool,
    limits: SourceLimits,
) -> Result<Program, Diagnostic> {
    let mut parsed = goal::parse(source, first, true).map_err(parser_diagnostic)?;
    let adapter = Adapter {
        source: &parsed.text,
        comments: std::mem::take(&mut parsed.comments),
        workflow,
        max_nesting: limits.max_nesting(),
        goal: parsed.goal,
        respelled_awaits: parsed.respelled_awaits,
        respellings: parsed.respellings,
        ..Adapter::default()
    };
    let converted = adapter.convert_module_items(&parsed.items);
    if converted.is_err() && adapter.module_goal_await.get() {
        // `await` written as an identifier: the cell is a Script.
        return parse_from_goal(source, Goal::Script, workflow, limits);
    }
    let early_error =
        matches!(&converted, Err(diagnostic) if diagnostic.kind == DiagnosticKind::ProgramDefect);
    if !early_error && let Some(error) = adapter.unvalidated_respelling() {
        return Err(parser_diagnostic(error.clone()));
    }
    let program = Program {
        statements: converted?,
        imported: adapter.imported.take(),
        annotation_limits: limits.entry_annotation_limits(),
        captions: adapter.captions.take(),
        comments: adapter.comments.ranges().to_vec(),
    };
    if workflow {
        // Two entries of one name break the workflow contract before they
        // break JavaScript's redeclaration rule.
        workflow::entries_once(&program)?;
    }
    declarations::check(&program)?;
    Ok(program)
}

#[derive(Default)]
struct Adapter<'a> {
    imported: RefCell<BTreeMap<String, Vec<String>>>,
    max_nesting: usize,
    /// The text SWC read, whose byte offsets every span indexes.
    source: &'a str,
    workflow: bool,
    /// A workflow module's comments; none for a cell.
    comments: SourceComments,
    /// The captions the comments spell, by what each names.
    captions: RefCell<Spelled>,
    goal: Goal,
    /// Where the Script-goal parse read a placeholder for `await`.
    respelled_awaits: BTreeSet<u32>,
    /// Every respelled word's position, with the error SWC reported there.
    respellings: BTreeMap<u32, swc_ecma_parser::error::Error>,
    /// The respelled words the adapter has read as identifiers or names.
    validated_respellings: RefCell<BTreeSet<u32>>,
    /// Whether the adapter stands in an async function's body or parameters.
    in_async_function: Cell<bool>,
    /// Set when `await` was refused as an identifier only because the Module
    /// goal reserves it: the cell is then read again as a Script.
    module_goal_await: Cell<bool>,
    /// Whether the adapter stands in an arrow function's parameters, where an
    /// `await` expression is an early error.
    in_arrow_parameters: Cell<bool>,
    nesting_depth: Cell<usize>,
    enum_constants: RefCell<Vec<BTreeMap<String, BTreeMap<String, ConstEnumValue>>>>,
    inline_enums: RefCell<Vec<BTreeSet<String>>>,
    enum_context: RefCell<Option<(String, BTreeSet<String>)>>,
}

impl Adapter<'_> {
    fn with_statement_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        self.with_depth(span, convert)
    }

    fn with_expression_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        self.with_depth(span, convert)
    }

    fn with_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let next = self.nesting_depth.get() + 1;
        if next > self.max_nesting {
            return Err(source_nesting_diagnostic(span, self.max_nesting));
        }
        self.nesting_depth.set(next);
        let result = convert();
        self.nesting_depth.set(next - 1);
        result
    }

    fn convert_module_items(&self, items: &[swc::ModuleItem]) -> Result<Vec<Stmt>, Diagnostic> {
        self.with_enum_scope(|| {
            items
                .iter()
                .map(|item| match item {
                    swc::ModuleItem::Stmt(stmt) => self.convert_listed(stmt, None),
                    swc::ModuleItem::ModuleDecl(swc::ModuleDecl::Import(import)) => {
                        let module = import.src.value.to_string_lossy();
                        let path = module.split('.').map(str::to_owned).collect::<Vec<_>>();
                        if path
                            .iter()
                            .any(|part| !lash_vocabulary::Segment::is_canonical(part))
                        {
                            return Err(reject(
                                DiagnosticCode::ImportExportUnsupported,
                                "a module path outside the advertised packages",
                                Some(source_span(import.span)),
                            ));
                        }
                        for specifier in &import.specifiers {
                            let (local, member) = match specifier {
                                swc::ImportSpecifier::Named(named) => {
                                    let member = named
                                        .imported
                                        .as_ref()
                                        .map(|name| match name {
                                            swc::ModuleExportName::Ident(id) => id.sym.to_string(),
                                            swc::ModuleExportName::Str(text) => {
                                                text.value.to_string_lossy().into_owned()
                                            }
                                        })
                                        .unwrap_or_else(|| named.local.sym.to_string());
                                    (named.local.sym.to_string(), Some(member))
                                }
                                swc::ImportSpecifier::Namespace(namespace) => {
                                    (namespace.local.sym.to_string(), None)
                                }
                                swc::ImportSpecifier::Default(default) => {
                                    (default.local.sym.to_string(), Some("default".to_owned()))
                                }
                            };
                            let mut target = path.clone();
                            target.extend(member);
                            if self.imported.borrow_mut().insert(local, target).is_some() {
                                return Err(reject(
                                    DiagnosticCode::ImportExportUnsupported,
                                    "a duplicate imported binding",
                                    Some(source_span(import.span)),
                                ));
                            }
                        }
                        Ok(Stmt::Empty)
                    }
                    swc::ModuleItem::ModuleDecl(swc::ModuleDecl::ExportDecl(export))
                        if self.workflow =>
                    {
                        self.convert_listed(
                            &swc::Stmt::Decl(export.decl.clone()),
                            Some(export.span.lo),
                        )
                    }
                    swc::ModuleItem::ModuleDecl(decl) => Err(reject(
                        DiagnosticCode::ImportExportUnsupported,
                        "static import/export declarations",
                        Some(source_span(decl.span())),
                    )),
                })
                .collect()
        })
    }

    fn convert_statements(&self, statements: &[swc::Stmt]) -> Result<Vec<Stmt>, Diagnostic> {
        self.with_enum_scope(|| {
            statements
                .iter()
                .map(|stmt| self.convert_listed(stmt, None))
                .collect()
        })
    }

    fn convert_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        let converted = self.convert_unspanned_stmt(stmt)?;
        Ok(Stmt::Spanned(source_span(stmt.span()), Box::new(converted)))
    }

    /// The body of an `if`/`else` or a loop is a Statement, never a
    /// Declaration. Strict code makes `if (x) function f() {}` the early
    /// SyntaxError that Annex B reserves for sloppy mode, and an `async
    /// function` declaration is a SyntaxError in either mode — but SWC still
    /// hands the async form to the adapter as a declaration.
    fn convert_body_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        if let swc::Stmt::Decl(swc::Decl::Fn(decl)) = stmt {
            return Err(Diagnostic::new(
                DiagnosticCode::SyntaxError,
                if decl.function.is_async {
                    "async functions can only be declared at the top level or inside a block"
                } else {
                    "in strict mode code, functions can only be declared at top level or inside a block"
                },
                Some(source_span(stmt.span())),
            ));
        }
        self.convert_stmt(stmt)
    }

    fn convert_unspanned_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        if !matches!(
            stmt,
            swc::Stmt::If(_)
                | swc::Stmt::While(_)
                | swc::Stmt::DoWhile(_)
                | swc::Stmt::For(_)
                | swc::Stmt::ForIn(_)
                | swc::Stmt::ForOf(_)
                | swc::Stmt::Switch(_)
                | swc::Stmt::Try(_)
                | swc::Stmt::Decl(_)
        ) {
            return self.convert_stmt_inner(stmt);
        }
        self.with_statement_depth(Some(source_span(stmt.span())), || {
            self.convert_stmt_inner(stmt)
        })
    }

    fn convert_stmt_inner(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        let span = Some(source_span(stmt.span()));
        Ok(match stmt {
            swc::Stmt::Empty(_) => Stmt::Empty,
            swc::Stmt::Expr(stmt) => Stmt::Expr(self.convert_expr(&stmt.expr)?),
            swc::Stmt::Block(block) => Stmt::Block(self.convert_statements(&block.stmts)?),
            swc::Stmt::Return(stmt) => Stmt::Return(
                stmt.arg
                    .as_deref()
                    .map(|expr| self.convert_expr(expr))
                    .transpose()?,
            ),
            swc::Stmt::If(stmt) => {
                self.caption_else(stmt);
                Stmt::If {
                    test: self.convert_expr(&stmt.test)?,
                    consequent: Box::new(self.convert_body_stmt(&stmt.cons)?),
                    alternate: stmt
                        .alt
                        .as_deref()
                        .map(|stmt| self.convert_body_stmt(stmt).map(Box::new))
                        .transpose()?,
                }
            }
            swc::Stmt::While(stmt) => Stmt::While {
                test: self.convert_expr(&stmt.test)?,
                body: Box::new(self.convert_body_stmt(&stmt.body)?),
            },
            swc::Stmt::Break(_) => Stmt::Break,
            swc::Stmt::Continue(_) => Stmt::Continue,
            swc::Stmt::Throw(stmt) => Stmt::Throw(self.convert_expr(&stmt.arg)?),
            swc::Stmt::Try(stmt) => {
                self.caption_clauses(stmt);
                Stmt::Try {
                    body: self.convert_statements(&stmt.block.stmts)?,
                    catch: stmt
                        .handler
                        .as_ref()
                        .map(|handler| {
                            Ok(Catch {
                                binding: handler
                                    .param
                                    .as_ref()
                                    .map(|pattern| self.convert_pattern(pattern))
                                    .transpose()?,
                                body: self.convert_statements(&handler.body.stmts)?,
                            })
                        })
                        .transpose()?,
                    finally: stmt
                        .finalizer
                        .as_ref()
                        .map(|block| self.convert_statements(&block.stmts))
                        .transpose()?,
                }
            }
            swc::Stmt::Decl(decl) => return self.convert_decl(decl),
            swc::Stmt::With(_) => {
                return Err(reject(
                    DiagnosticCode::WithUnsupported,
                    "with statements",
                    span,
                ));
            }
            swc::Stmt::Labeled(labeled) => {
                self.identifier(&labeled.label)?;
                return Err(Diagnostic::new(
                    DiagnosticCode::LabelUnsupported,
                    "Unsupported: labeled break/continue. Extract the labeled region into a helper function and return.",
                    span,
                ));
            }
            swc::Stmt::Switch(stmt) => Stmt::Switch {
                discriminant: self.convert_expr(&stmt.discriminant)?,
                cases: stmt
                    .cases
                    .iter()
                    .map(|case| {
                        Ok(SwitchCase {
                            test: case
                                .test
                                .as_deref()
                                .map(|test| self.convert_expr(test))
                                .transpose()?,
                            consequent: self.convert_statements(&case.cons)?,
                        })
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            },
            swc::Stmt::DoWhile(stmt) => Stmt::DoWhile {
                body: Box::new(self.convert_body_stmt(&stmt.body)?),
                test: self.convert_expr(&stmt.test)?,
                test_span: source_span(stmt.test.span()),
            },
            swc::Stmt::For(stmt) => {
                self.check_for_head(stmt)?;
                Stmt::For {
                    init: stmt
                        .init
                        .as_ref()
                        .map(|init| match init {
                            swc::VarDeclOrExpr::VarDecl(decl) => self
                                .convert_decl(&swc::Decl::Var(decl.clone()))
                                .map(Box::new),
                            swc::VarDeclOrExpr::Expr(expr) => self
                                .convert_expr(expr)
                                .map(|expr| Box::new(Stmt::Expr(expr))),
                        })
                        .transpose()?,
                    test: stmt
                        .test
                        .as_deref()
                        .map(|expr| self.convert_expr(expr))
                        .transpose()?,
                    update: stmt
                        .update
                        .as_deref()
                        .map(|expr| self.convert_expr(expr))
                        .transpose()?,
                    body: Box::new(self.convert_body_stmt(&stmt.body)?),
                }
            }
            swc::Stmt::ForIn(stmt) => {
                let (pattern, kind) = self.convert_for_head(&stmt.left, span)?;
                Stmt::ForIn {
                    pattern,
                    kind,
                    object: self.convert_expr(&stmt.right)?,
                    body: Box::new(self.convert_body_stmt(&stmt.body)?),
                }
            }
            swc::Stmt::ForOf(stmt) => {
                self.check_for_of_keyword(stmt.left.span(), stmt.right.span())?;
                if stmt.is_await {
                    return Err(reject(
                        DiagnosticCode::ForOfUnsupported,
                        "for await/of statements",
                        span,
                    ));
                }
                let (pattern, kind) = self.convert_for_head(&stmt.left, span)?;
                Stmt::ForOf {
                    pattern,
                    kind,
                    iterable: self.convert_expr(&stmt.right)?,
                    body: Box::new(self.convert_body_stmt(&stmt.body)?),
                }
            }
            swc::Stmt::Debugger(_) => {
                return Err(reject(
                    DiagnosticCode::DebuggerUnsupported,
                    "debugger statements",
                    span,
                ));
            }
        })
    }

    fn convert_decl(&self, decl: &swc::Decl) -> Result<Stmt, Diagnostic> {
        let span = Some(source_span(decl.span()));
        match decl {
            swc::Decl::Class(class) if !class.class.decorators.is_empty() => Err(reject(
                DiagnosticCode::DecoratorUnsupported,
                "decorators",
                span,
            )),
            swc::Decl::Class(class) => {
                self.identifier(&class.ident)?;
                Err(Diagnostic::new(
                    DiagnosticCode::ClassUnsupported,
                    "Unsupported: classes. Use functions and plain objects; for coded errors use Object.assign(new Error(message), {code}).",
                    span,
                ))
            }
            swc::Decl::TsEnum(declaration) => self.convert_enum(declaration),
            swc::Decl::TsModule(_) => Err(reject(
                DiagnosticCode::NamespaceUnsupported,
                "TypeScript namespaces/modules",
                span,
            )),
            swc::Decl::Using(_) => Err(reject(
                DiagnosticCode::UsingUnsupported,
                "using declarations",
                span,
            )),
            swc::Decl::TsTypeAlias(alias) => Ok(Stmt::TypeAlias {
                name: alias.id.sym.to_string(),
                ty: types::convert_alias(alias),
            }),
            swc::Decl::TsInterface(interface) => Ok(Stmt::TypeAlias {
                name: interface.id.sym.to_string(),
                ty: types::convert_interface(interface),
            }),
            swc::Decl::Var(decl) => {
                if decl.declare {
                    return Err(reject(
                        DiagnosticCode::DeclareUnsupported,
                        "ambient declare declarations",
                        span,
                    ));
                }
                let kind = convert_var_kind(decl.kind);
                let declarations = decl
                    .decls
                    .iter()
                    .map(|decl| {
                        Ok(Var {
                            pattern: self.convert_pattern(&decl.name)?,
                            init: decl
                                .init
                                .as_deref()
                                .map(|expr| self.convert_expr(expr))
                                .transpose()?,
                        })
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Stmt::Var { kind, declarations })
            }
            swc::Decl::Fn(decl) => {
                if decl.declare {
                    return Err(reject(
                        DiagnosticCode::DeclareUnsupported,
                        "ambient declare declarations",
                        span,
                    ));
                }
                if !decl.function.decorators.is_empty() {
                    return Err(reject(
                        DiagnosticCode::DecoratorUnsupported,
                        "decorators",
                        span,
                    ));
                }
                let name = self.identifier(&decl.ident)?;
                let function = self.convert_function(Some(name.clone()), &decl.function)?;
                Ok(Stmt::Function { name, function })
            }
        }
    }

    fn convert_for_head(
        &self,
        head: &swc::ForHead,
        span: Option<SourceSpan>,
    ) -> Result<(Pattern, Option<VarKind>), Diagnostic> {
        match head {
            swc::ForHead::VarDecl(decl) => {
                let [declaration] = decl.decls.as_slice() else {
                    return Err(reject(
                        DiagnosticCode::ForOfUnsupported,
                        "for-in/of heads with multiple declarations",
                        span,
                    ));
                };
                if declaration.init.is_some() {
                    return Err(reject(
                        DiagnosticCode::ForOfUnsupported,
                        "for-in/of head initializers",
                        span,
                    ));
                }
                Ok((
                    self.convert_pattern(&declaration.name)?,
                    Some(convert_var_kind(decl.kind)),
                ))
            }
            swc::ForHead::Pat(pattern) => {
                let pattern = self.convert_pattern(pattern)?;
                self.check_assignment_pattern(&pattern, span)?;
                Ok((pattern, None))
            }
            swc::ForHead::UsingDecl(_) => Err(reject(
                DiagnosticCode::UsingUnsupported,
                "using declarations in for-in/of heads",
                span,
            )),
        }
    }

    fn convert_pattern(&self, pattern: &swc::Pat) -> Result<Pattern, Diagnostic> {
        self.with_expression_depth(Some(source_span(pattern.span())), || {
            self.convert_pattern_inner(pattern)
        })
    }

    fn convert_pattern_inner(&self, pattern: &swc::Pat) -> Result<Pattern, Diagnostic> {
        let span = Some(source_span(pattern.span()));
        Ok(match pattern {
            swc::Pat::Ident(name) => {
                Pattern::Ident(self.identifier(&name.id)?, types::convert_binding(name))
            }
            swc::Pat::Expr(expr) => match unasserted(self.convert_expr(expr)?) {
                Expr::Ident(name, _) => Pattern::Ident(name, None),
                Expr::Member {
                    object, property, ..
                } => Pattern::Member { object, property },
                _ => {
                    return Err(reject_defect(
                        DiagnosticCode::UnsupportedExpression,
                        "invalid destructuring assignment targets",
                        span,
                    ));
                }
            },
            swc::Pat::Assign(pattern) => Pattern::Assign {
                target: Box::new(self.convert_pattern(&pattern.left)?),
                default: Box::new(self.convert_expr(&pattern.right)?),
            },
            swc::Pat::Rest(pattern) => Pattern::Rest(Box::new(self.convert_pattern(&pattern.arg)?)),
            swc::Pat::Array(array) => {
                let mut elements = Vec::new();
                let mut rest = None;
                for (index, element) in array.elems.iter().enumerate() {
                    match element {
                        Some(swc::Pat::Rest(pattern)) => {
                            if index + 1 != array.elems.len() {
                                return Err(reject(
                                    DiagnosticCode::SyntaxError,
                                    "rest elements before the end of an array pattern",
                                    span,
                                ));
                            }
                            rest = Some(Box::new(self.convert_pattern(&pattern.arg)?));
                        }
                        Some(pattern) => elements.push(Some(self.convert_pattern(pattern)?)),
                        None => elements.push(None),
                    }
                }
                Pattern::Array { elements, rest }
            }
            swc::Pat::Object(object) => {
                let mut properties = Vec::new();
                let mut rest = None;
                for property in &object.props {
                    match property {
                        swc::ObjectPatProp::KeyValue(property) => {
                            properties.push(ObjectPatternProperty {
                                key: self.convert_property_key(&property.key)?,
                                value: self.convert_pattern(&property.value)?,
                            });
                        }
                        swc::ObjectPatProp::Assign(property) => {
                            let name = self.identifier(&property.key.id)?;
                            let target = Pattern::Ident(name.clone(), None);
                            let value = match property.value.as_deref() {
                                Some(default) => Pattern::Assign {
                                    target: Box::new(target),
                                    default: Box::new(self.convert_expr(default)?),
                                },
                                None => target,
                            };
                            properties.push(ObjectPatternProperty {
                                key: PropertyKey::Static(name),
                                value,
                            });
                        }
                        swc::ObjectPatProp::Rest(property) => {
                            rest = Some(Box::new(self.convert_pattern(&property.arg)?));
                        }
                    }
                }
                Pattern::Object { properties, rest }
            }
            swc::Pat::Invalid(_) => {
                return Err(reject_defect(
                    DiagnosticCode::UnsupportedExpression,
                    "invalid binding patterns",
                    span,
                ));
            }
        })
    }

    fn convert_property_key(&self, name: &swc::PropName) -> Result<PropertyKey, Diagnostic> {
        check_property_key(name)?;
        Ok(match name {
            swc::PropName::Ident(name) => PropertyKey::Static(self.identifier_name(name)?),
            swc::PropName::Str(name) => {
                self.check_string_literal(name.span)?;
                PropertyKey::Static(name.value.to_string_lossy().into_owned())
            }
            swc::PropName::Num(name) => PropertyKey::Static(name.value.to_string()),
            swc::PropName::Computed(name) => {
                PropertyKey::Computed(Box::new(self.convert_expr(&name.expr)?))
            }
            swc::PropName::BigInt(value) => PropertyKey::Static(value.value.to_string()),
        })
    }

    fn convert_expr(&self, expr: &swc::Expr) -> Result<Expr, Diagnostic> {
        self.with_expression_depth(Some(source_span(expr.span())), || {
            self.convert_expr_inner(expr)
        })
    }

    fn convert_expr_inner(&self, expr: &swc::Expr) -> Result<Expr, Diagnostic> {
        let span = Some(source_span(expr.span()));
        Ok(match expr {
            swc::Expr::Ident(ident) => {
                let name = self.identifier(ident)?;
                if let Some((enum_name, members)) = self.enum_context.borrow().as_ref()
                    && members.contains(&name)
                {
                    Expr::Member {
                        object: Box::new(Expr::Ident(enum_name.clone(), None)),
                        property: MemberProperty::Field(name),
                        span: source_span(expr.span()),
                    }
                } else {
                    Expr::Ident(name, span)
                }
            }
            swc::Expr::Lit(lit) => match lit {
                swc::Lit::Null(_) => Expr::Null,
                swc::Lit::Bool(value) => Expr::Bool(value.value),
                swc::Lit::Num(value) => Expr::Number(value.value),
                swc::Lit::Str(value) => {
                    self.check_string_literal(value.span)?;
                    value
                        .value
                        .as_str()
                        .map_or(Expr::LoneSurrogateString, |value| {
                            Expr::String(value.to_string())
                        })
                }
                swc::Lit::Regex(value) => {
                    let pattern = value.exp.to_string();
                    let flags = value.flags.to_string();
                    early_errors::check_regex_body(&pattern, span)?;
                    Expr::RegExp { pattern, flags }
                }
                swc::Lit::BigInt(value) => {
                    Expr::BigInt(lash_kernel_doc::Integer::new((*value.value).clone()))
                }
                swc::Lit::JSXText(_) => {
                    return Err(reject(DiagnosticCode::JsxUnsupported, "JSX", span));
                }
            },
            swc::Expr::Array(array) => Expr::Array(
                array
                    .elems
                    .iter()
                    .map(|element| {
                        let Some(element) = element else {
                            return Ok(ArrayElement::Hole);
                        };
                        let value = self.convert_expr(&element.expr)?;
                        Ok(if element.spread.is_some() {
                            ArrayElement::Spread(value)
                        } else {
                            ArrayElement::Value(value)
                        })
                    })
                    .collect::<Result<_, _>>()?,
            ),
            swc::Expr::Object(object) => Expr::Object(
                object
                    .props
                    .iter()
                    .map(|property| self.convert_property(property))
                    .collect::<Result<_, _>>()?,
            ),
            swc::Expr::Fn(function) => {
                // A function expression's own name is read under the rule of
                // its body (§15.2, §15.8).
                let name = self.in_function(function.function.is_async, || {
                    function
                        .ident
                        .as_ref()
                        .map(|name| self.identifier(name))
                        .transpose()
                })?;
                Expr::Function(self.convert_function(name, &function.function)?)
            }
            swc::Expr::Arrow(function) => self.convert_arrow(function, span)?,
            swc::Expr::Unary(expr) => {
                let op = match expr.op {
                    swc::UnaryOp::Plus => UnaryOp::Plus,
                    swc::UnaryOp::Minus => UnaryOp::Minus,
                    swc::UnaryOp::Bang => UnaryOp::Not,
                    swc::UnaryOp::TypeOf => UnaryOp::TypeOf,
                    swc::UnaryOp::Void => UnaryOp::Void,
                    swc::UnaryOp::Delete => return self.convert_delete(&expr.arg, span),
                    swc::UnaryOp::Tilde => UnaryOp::BitNot,
                };
                Expr::Unary {
                    op,
                    value: Box::new(self.convert_expr(&expr.arg)?),
                }
            }
            swc::Expr::Bin(expr) => self.convert_binary(expr)?,
            swc::Expr::Assign(expr) => {
                self.check_assignment_repair(expr)?;
                Expr::Assign {
                    target: self.convert_assign_target(&expr.left)?,
                    op: convert_assign_op(expr.op),
                    value: Box::new(self.convert_expr(&expr.right)?),
                }
            }
            swc::Expr::Member(member) => self.convert_member(member)?,
            swc::Expr::Cond(expr) => Expr::Conditional {
                test: Box::new(self.convert_expr(&expr.test)?),
                consequent: Box::new(self.convert_expr(&expr.cons)?),
                alternate: Box::new(self.convert_expr(&expr.alt)?),
            },
            swc::Expr::Call(call) => {
                let callee = match &call.callee {
                    swc::Callee::Expr(expr) => self.convert_expr(expr)?,
                    swc::Callee::Import(_) => {
                        return Err(reject(
                            DiagnosticCode::DynamicImportUnsupported,
                            "dynamic import",
                            span,
                        ));
                    }
                    swc::Callee::Super(_) => {
                        return Err(reject(
                            DiagnosticCode::SuperUnsupported,
                            "super calls",
                            span,
                        ));
                    }
                };
                if matches!(&callee, Expr::Ident(name, _) if name == "eval") {
                    return Err(reject(DiagnosticCode::EvalUnsupported, "eval", span));
                }
                if matches!(&callee, Expr::Ident(name, _) if name == "Function") {
                    return Err(reject(
                        DiagnosticCode::FunctionConstructorUnsupported,
                        "Function constructor",
                        span,
                    ));
                }
                let args = self.convert_call_args(&call.args)?;
                self.append_optional_operation(
                    callee,
                    OptionalOperation::Call {
                        args,
                        optional: false,
                    },
                    source_span(call.span),
                )
            }
            swc::Expr::Tpl(template) => self.convert_template(template)?,
            swc::Expr::Paren(expr) => optional_chain::parenthesized(self.convert_expr(&expr.expr)?),
            swc::Expr::TsTypeAssertion(expr) => Expr::As {
                value: Box::new(self.convert_expr(&expr.expr)?),
                ty: types::convert_type(&expr.type_ann),
            },
            swc::Expr::TsConstAssertion(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsNonNull(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsAs(expr) => Expr::As {
                value: Box::new(self.convert_expr(&expr.expr)?),
                ty: types::convert_type(&expr.type_ann),
            },
            swc::Expr::TsInstantiation(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsSatisfies(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::This(_) => Expr::This,
            swc::Expr::Update(update) => Expr::Update {
                target: self.convert_update_target(&update.arg)?,
                delta: if update.op == swc::UpdateOp::PlusPlus {
                    1.0
                } else {
                    -1.0
                },
                prefix: update.prefix,
            },
            swc::Expr::New(new) => {
                let swc::Expr::Ident(constructor) = new.callee.as_ref() else {
                    if let swc::Expr::Member(member) = new.callee.as_ref()
                        && let swc::Expr::Ident(root) = member.obj.as_ref()
                        && root.sym == "Intl"
                    {
                        let method = match &member.prop {
                            swc::MemberProp::Ident(method) => method.sym.as_ref(),
                            _ => "computed method",
                        };
                        return Err(Diagnostic::refusal(
                            DiagnosticCode::MethodUnsupported,
                            format!(
                                "Unsupported: Intl.{method}; use toLocaleString() or toLocaleString('en-US') with no options; Date uses UTC"
                            ),
                            span,
                        ));
                    }
                    return Err(reject(
                        DiagnosticCode::NewUnsupported,
                        "new with a non-built-in constructor",
                        span,
                    ));
                };
                Expr::New {
                    constructor: self.identifier(constructor)?,
                    args: self.convert_call_args(new.args.as_deref().unwrap_or_default())?,
                }
            }
            swc::Expr::Seq(_) => {
                return Err(reject(
                    DiagnosticCode::SequenceUnsupported,
                    "sequence expressions",
                    span,
                ));
            }
            swc::Expr::TaggedTpl(template) => return Err(self.tagged_template_repair(template)),
            swc::Expr::Class(class) => {
                if let Some(name) = &class.ident {
                    self.identifier(name)?;
                }
                return Err(Diagnostic::new(
                    DiagnosticCode::ClassUnsupported,
                    "Unsupported: classes. Use functions and plain objects; for coded errors use Object.assign(new Error(message), {code}).",
                    span,
                ));
            }
            swc::Expr::Yield(_) => {
                return Err(reject(DiagnosticCode::YieldUnsupported, "yield", span));
            }
            swc::Expr::MetaProp(_) => {
                return Err(reject(
                    DiagnosticCode::MetaPropertyUnsupported,
                    "meta properties",
                    span,
                ));
            }
            swc::Expr::Await(await_expr) => {
                self.check_await_expression(await_expr.span)?;
                Expr::Await {
                    value: Box::new(self.convert_expr(&await_expr.arg)?),
                    span: source_span(await_expr.span),
                }
            }
            swc::Expr::SuperProp(_) => {
                return Err(reject(
                    DiagnosticCode::SuperUnsupported,
                    "super properties",
                    span,
                ));
            }
            swc::Expr::OptChain(chain) => self.convert_optional_chain(chain)?,
            swc::Expr::PrivateName(_) => {
                return Err(reject(
                    DiagnosticCode::PrivateNameUnsupported,
                    "private names",
                    span,
                ));
            }
            swc::Expr::JSXMember(_)
            | swc::Expr::JSXNamespacedName(_)
            | swc::Expr::JSXEmpty(_)
            | swc::Expr::JSXElement(_)
            | swc::Expr::JSXFragment(_) => {
                return Err(reject(DiagnosticCode::JsxUnsupported, "JSX", span));
            }
            swc::Expr::Invalid(_) => {
                return Err(reject_defect(
                    DiagnosticCode::UnsupportedExpression,
                    "invalid expression",
                    span,
                ));
            }
        })
    }

    fn convert_property(&self, property: &swc::PropOrSpread) -> Result<ObjectProperty, Diagnostic> {
        let swc::PropOrSpread::Prop(property) = property else {
            let swc::PropOrSpread::Spread(spread) = property else {
                unreachable!()
            };
            return Ok(ObjectProperty::Spread(self.convert_expr(&spread.expr)?));
        };
        match property.as_ref() {
            swc::Prop::Shorthand(name) => {
                let name_text = self.identifier(name)?;
                Ok(ObjectProperty::KeyValue(
                    PropertyKey::Static(name_text.clone()),
                    Expr::Ident(name_text, Some(source_span(name.span))),
                ))
            }
            swc::Prop::KeyValue(property) => Ok(ObjectProperty::KeyValue(
                self.convert_property_key(&property.key)?,
                self.convert_expr(&property.value)?,
            )),
            swc::Prop::Getter(_) | swc::Prop::Setter(_) => Err(reject(
                DiagnosticCode::AccessorUnsupported,
                "getters/setters",
                Some(source_span(property.span())),
            )),
            swc::Prop::Method(method) => {
                self.check_async_method_head(method)?;
                Ok(ObjectProperty::KeyValue(
                    self.convert_property_key(&method.key)?,
                    Expr::Function(self.convert_function(None, &method.function)?),
                ))
            }
            swc::Prop::Assign(_) => Err(reject_refusal(
                DiagnosticCode::UnsupportedExpression,
                "assignment properties",
                Some(source_span(property.span())),
            )),
        }
    }

    fn convert_binary(&self, expr: &swc::BinExpr) -> Result<Expr, Diagnostic> {
        use swc::BinaryOp as S;
        let left = Box::new(self.convert_expr(&expr.left)?);
        let right = Box::new(self.convert_expr(&expr.right)?);
        Ok(match expr.op {
            S::LogicalAnd => Expr::Logical {
                left,
                op: LogicalOp::And,
                right,
            },
            S::LogicalOr => Expr::Logical {
                left,
                op: LogicalOp::Or,
                right,
            },
            S::NullishCoalescing => Expr::Logical {
                left,
                op: LogicalOp::Nullish,
                right,
            },
            op => {
                let op = match op {
                    S::Add => BinaryOp::Add,
                    S::Sub => BinaryOp::Subtract,
                    S::Mul => BinaryOp::Multiply,
                    S::Div => BinaryOp::Divide,
                    S::Mod => BinaryOp::Remainder,
                    S::Exp => BinaryOp::Exponent,
                    S::BitAnd => BinaryOp::BitAnd,
                    S::BitOr => BinaryOp::BitOr,
                    S::BitXor => BinaryOp::BitXor,
                    S::LShift => BinaryOp::ShiftLeft,
                    S::RShift => BinaryOp::ShiftRight,
                    S::ZeroFillRShift => BinaryOp::ShiftRightUnsigned,
                    S::In => BinaryOp::In,
                    S::InstanceOf => BinaryOp::InstanceOf,
                    S::EqEqEq => BinaryOp::StrictEqual,
                    S::NotEqEq => BinaryOp::StrictNotEqual,
                    S::EqEq => BinaryOp::LooseEqual,
                    S::NotEq => BinaryOp::LooseNotEqual,
                    S::Lt => BinaryOp::Less,
                    S::LtEq => BinaryOp::LessEqual,
                    S::Gt => BinaryOp::Greater,
                    S::GtEq => BinaryOp::GreaterEqual,
                    S::LogicalOr | S::LogicalAnd | S::NullishCoalescing => {
                        unreachable!("logical operators are classified above")
                    }
                };
                Expr::Binary {
                    operand_spans: Some([
                        source_span(expr.left.span()),
                        source_span(expr.right.span()),
                    ]),
                    left,
                    op,
                    right,
                }
            }
        })
    }

    fn convert_member(&self, member: &swc::MemberExpr) -> Result<Expr, Diagnostic> {
        if let swc::Expr::Ident(owner) = member.obj.as_ref()
            && self.enum_is_inline(owner.sym.as_ref())
            && let Some(name) = enum_member_property_name(&member.prop)
            && let Some(value) = self.enum_constant(owner.sym.as_ref(), &name)
        {
            return Ok(value.expression());
        }
        let object = self.convert_expr(&member.obj)?;
        if matches!(&object, Expr::Ident(name, _) if name == "prototype") {
            return Err(self.prototype_member_repair(member));
        }
        let property = match &member.prop {
            swc::MemberProp::Ident(name) => {
                if is_prototype_chain_property(name.sym.as_ref())
                    && !(name.sym.as_ref() == "prototype"
                        && matches!(&member.obj.as_ref(), swc::Expr::Ident(owner) if crate::builtins::is_global(owner.sym.as_ref())))
                {
                    return Err(self.prototype_member_repair(member));
                }
                MemberProperty::Field(self.identifier_name(name)?)
            }
            swc::MemberProp::Computed(property) => {
                if let swc::Expr::Lit(swc::Lit::Str(name)) = property.expr.as_ref()
                    && is_prototype_chain_property(&name.value.to_string_lossy())
                {
                    return Err(self.prototype_member_repair(member));
                }
                MemberProperty::Index(Box::new(self.convert_expr(&property.expr)?))
            }
            swc::MemberProp::PrivateName(_) => {
                return Err(reject(
                    DiagnosticCode::PrivateNameUnsupported,
                    "private names",
                    Some(source_span(member.span)),
                ));
            }
        };
        Ok(self.append_optional_operation(
            object,
            OptionalOperation::Member {
                property,
                optional: false,
            },
            source_span(member.span),
        ))
    }

    fn convert_call_args(&self, args: &[swc::ExprOrSpread]) -> Result<Vec<CallArg>, Diagnostic> {
        args.iter()
            .map(|arg| {
                let value = self.convert_expr(&arg.expr)?;
                Ok(if arg.spread.is_some() {
                    CallArg::Spread(value)
                } else {
                    CallArg::Value(value)
                })
            })
            .collect()
    }
}

fn convert_var_kind(kind: swc::VarDeclKind) -> VarKind {
    match kind {
        swc::VarDeclKind::Var => VarKind::Var,
        swc::VarDeclKind::Let => VarKind::Let,
        swc::VarDeclKind::Const => VarKind::Const,
    }
}

fn convert_assign_op(op: swc::AssignOp) -> AssignOp {
    use swc::AssignOp as S;
    match op {
        S::Assign => AssignOp::Assign,
        S::AddAssign => AssignOp::Binary(BinaryOp::Add),
        S::SubAssign => AssignOp::Binary(BinaryOp::Subtract),
        S::MulAssign => AssignOp::Binary(BinaryOp::Multiply),
        S::DivAssign => AssignOp::Binary(BinaryOp::Divide),
        S::ModAssign => AssignOp::Binary(BinaryOp::Remainder),
        S::ExpAssign => AssignOp::Binary(BinaryOp::Exponent),
        S::BitAndAssign => AssignOp::Binary(BinaryOp::BitAnd),
        S::BitOrAssign => AssignOp::Binary(BinaryOp::BitOr),
        S::BitXorAssign => AssignOp::Binary(BinaryOp::BitXor),
        S::LShiftAssign => AssignOp::Binary(BinaryOp::ShiftLeft),
        S::RShiftAssign => AssignOp::Binary(BinaryOp::ShiftRight),
        S::ZeroFillRShiftAssign => AssignOp::Binary(BinaryOp::ShiftRightUnsigned),
        S::AndAssign => AssignOp::Logical(LogicalOp::And),
        S::OrAssign => AssignOp::Logical(LogicalOp::Or),
        S::NullishAssign => AssignOp::Logical(LogicalOp::Nullish),
    }
}
