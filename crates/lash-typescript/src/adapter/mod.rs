use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crate::node_label::{NodeLabel, is_label_comment, parse_label_comment};
use crate::{Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};
use swc_common::comments::{CommentKind, Comments, SingleThreadedComments};
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as swc;

mod declarations;
mod early_errors;
mod enums;
mod goal;
mod nesting;
mod optional_chain;
mod prototype_chain;
mod rejections;
mod template;
#[cfg(test)]
mod tests;
mod traversal;
mod types;
use enums::{ConstEnumValue, enum_member_property_name};
use goal::Goal;
use nesting::{guard_source_nesting, source_nesting_diagnostic};
pub(crate) use prototype_chain::is_prototype_chain_property as names_the_prototype_chain;
use prototype_chain::{
    builtin_prototype_mutation, check_property_key, is_prototype_chain_property,
    prototype_access_rejection,
};
use rejections::{parser_diagnostic, reject, reject_defect, reject_refusal, source_span};
pub(crate) use types::{TypeAnnotation, TypeAnnotationField, TypeShape};

/// This is deliberately below the shared AST and 2 MiB native-stack limits.
pub const MAX_SOURCE_NESTING_DEPTH: usize = 28;

#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub(crate) statements: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub(crate) enum Stmt {
    Spanned(SourceSpan, Box<Stmt>),
    Empty,
    /// A statement carrying the label its leading `@label` doc comment named.
    ///
    /// The wrapper is inert: it names the statement below it and changes
    /// nothing about how that statement runs.
    Labeled {
        label: NodeLabel,
        stmt: Box<Stmt>,
    },
    Expr(Expr),
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
    /// The statement itself, with any `@label` wrapper peeled off.
    ///
    /// The label names a node; it never changes what the statement *is*, so
    /// every pass that asks what kind of statement this is — hoisting,
    /// pre-declaration, loop analysis — asks through here.
    pub(crate) fn unlabeled(&self) -> &Self {
        match self {
            Self::Spanned(_, stmt) | Self::Labeled { stmt, .. } => stmt.unlabeled(),
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
    /// A bound name, with the type annotation written on it if there was one.
    /// The annotation is decorative everywhere but a process-literal
    /// parameter, where it becomes the process's declared input type.
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
    pub(crate) body: FunctionBody,
    pub(crate) is_async: bool,
    /// An arrow has no receiver of its own: its `this` is its enclosing
    /// function's (ECMA-262 lexical `this`), and its `arguments` binds the
    /// enclosing non-arrow function's arguments object, not one of its own.
    pub(crate) is_arrow: bool,
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
        expressions: Vec<Expr>,
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

#[derive(Clone, Copy, Debug)]
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

/// The largest TypeScript cell this dialect will read. Sources above it reject
/// with `TS_SOURCE_TOO_LARGE`; the bound is what makes the parse stack
/// reservation below finite. 64 KiB is roughly 1 600 lines of TypeScript.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;

/// Stack reserved before any source-proportional allowance.
///
/// It covers the parser's fixed frames and everything downstream of the parse,
/// which is bounded independently of the source: [`Adapter`] refuses to convert
/// past [`MAX_SOURCE_NESTING_DEPTH`], so the normalized tree it produces is at
/// most that deep, the lowerer walks that tree, and `lashlang` then rejects any
/// shared AST deeper than its own `MAX_AST_NESTING_DEPTH`. Only the parse and
/// the drop of its output scale with the source, which is what the allowance
/// below pays for.
const PARSE_STACK_BASE_BYTES: usize = 8 * 1024 * 1024;

/// Parser stack reserved per source byte.
///
/// The honest worst case is **one source byte per nesting level**. An earlier
/// derivation claimed two — an opener and a closer — and that is false: `(`
/// repeated with no closers is a complete recursive-descent recursion of depth
/// `n` from `n` bytes, and SWC only discovers the problem at end of input. Worse,
/// that same shape is the most expensive *per level*, so the densest source and
/// the deepest frames coincide rather than trading off.
///
/// Measured by binary search, each attempt in its own process, on the unclosed
/// forms:
///
/// | shape | bytes per level | bytes per source byte |
/// | --- | ---: | ---: |
/// | `(` unclosed | ~19 900 | **~19 900** |
/// | `A<` unclosed | ~19 100 | ~9 300 |
/// | `{` unclosed | ~12 000 | ~12 000 |
/// | `[` unclosed | ~11 300 | ~11 300 |
/// | `a:` labels | ~11 300 | ~5 600 |
/// | `(`…`)` closed | ~20 700 | ~10 400 |
///
/// The round-7 verification measured the same shape at up to **22 540** bytes
/// per source byte, which is the figure this constant is set against. Usage is
/// linear in depth — at depths 1 000, 2 000, 4 000 and 8 000 the per-level cost
/// varies by under half a percent — which is what makes extrapolating to the
/// bound sound.
///
/// So reserving 40 000 bytes per source byte leaves a margin of roughly
/// **1.8x** (40 000 / 22 540), not the 4x an earlier comment claimed. An
/// independent check agrees: the worst shape at the bound touches 1 228 MB of
/// the 2 508 MB reserved, a **2.04x** margin by peak RSS.
///
/// Two reasons that margin is accepted rather than widened. Raising the constant
/// to restore 4x would reserve 5.9 GiB for a cap-sized cell, which makes the
/// address-space requirement in the deviation register worse — the reservation
/// already fails closed on a host with `RLIMIT_AS` under 2 GiB. And the margin
/// is *guarded*, not asserted: `tests/no_abort_guarantee.rs` runs these worst
/// shapes filled to the bound with the nesting preflight disabled, so a future
/// SWC whose frames outgrew the reservation would abort there and fail CI rather
/// than in production.
///
/// With [`MAX_SOURCE_BYTES`] at 64 KiB the largest reservation is 8 MiB +
/// 2.44 GiB. That is address space rather than memory: pages commit only when
/// touched, and an ordinary cell touches a few hundred kilobytes.
const PARSE_STACK_BYTES_PER_SOURCE_BYTE: usize = 40_000;

/// The stack a source of this size is parsed on.
fn parse_stack_size(source_bytes: usize) -> usize {
    PARSE_STACK_BASE_BYTES + PARSE_STACK_BYTES_PER_SOURCE_BYTE * source_bytes
}

fn guard_source_size(source: &str) -> Result<(), Diagnostic> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Diagnostic::with_repair(
            DiagnosticCode::SourceTooLarge,
            format!(
                "TypeScript source is {} bytes, over the {MAX_SOURCE_BYTES}-byte limit",
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
    guard_source_size(source)?;
    guard_source_nesting(source)?;
    parse_on_proportional_stack(source)
}

/// The same path with the nesting preflight removed, so a test can demonstrate
/// that the no-abort guarantee rests on the stack reservation rather than on
/// the preflight agreeing with SWC about the shape of the source.
#[cfg(feature = "testing")]
pub(crate) fn parse_without_nesting_preflight(source: &str) -> Result<Program, Diagnostic> {
    guard_source_size(source)?;
    parse_on_proportional_stack(source)
}

/// Parse on the caller's own stack with no guard at all. Only a stack
/// measurement wants this; everything else goes through [`parse`].
#[cfg(feature = "testing")]
pub(crate) fn parse_unguarded(source: &str) -> Result<Program, Diagnostic> {
    parse_source(source)
}

fn parse_on_proportional_stack(source: &str) -> Result<Program, Diagnostic> {
    let stack_size = parse_stack_size(source.len());
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("typescript-parse".to_string())
            .stack_size(stack_size)
            .spawn_scoped(scope, || parse_source(source))
            .map_err(|error| {
                // The host could not give us the reservation. That is a
                // resource failure, not a defect in the program, and it must not
                // be reported as one: an operator reading `TS_INVALID_SHARED_AST`
                // would go and debug the cell instead of the address-space
                // limit. See MAX_SOURCE_BYTES for what the requirement is.
                Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    format!(
                        "the TypeScript parser could not reserve {stack_size} bytes of stack for a                          {}-byte source: {error}",
                        source.len()
                    ),
                    None,
                )
            })?;
        handle.join().unwrap_or_else(|_| {
            Err(Diagnostic::new(
                DiagnosticCode::SyntaxError,
                "the TypeScript parser failed while reading this source",
                None,
            ))
        })
    })
}

fn parse_source(source: &str) -> Result<Program, Diagnostic> {
    parse_from_goal(source, Goal::Module)
}

/// Reads `source` under the first goal, from `first` on, that admits it (see
/// [`Goal`]), converts it, and checks the early errors of its declarations.
fn parse_from_goal(source: &str, first: Goal) -> Result<Program, Diagnostic> {
    let parsed = goal::parse(source, first).map_err(|error| parser_diagnostic(error, source))?;
    let adapter = Adapter {
        source: &parsed.text,
        goal: parsed.goal,
        respelled_awaits: parsed.respelled_awaits,
        respellings: parsed.respellings,
        comments: parsed.comments.clone(),
        ..Adapter::default()
    };
    let converted = adapter.convert_module_items(&parsed.items);
    if converted.is_err() && adapter.module_goal_await.get() {
        // `await` written as an identifier: the cell is a Script.
        return parse_from_goal(source, Goal::Script);
    }
    let early_error =
        matches!(&converted, Err(diagnostic) if diagnostic.kind == DiagnosticKind::ProgramDefect);
    if !early_error && let Some(error) = adapter.unvalidated_respelling() {
        return Err(parser_diagnostic(error.clone(), source));
    }
    let program = Program {
        statements: converted?,
    };
    declarations::check(&program)?;
    Ok(program)
}

#[derive(Default)]
struct Adapter<'a> {
    /// The text SWC read, whose byte offsets every span indexes.
    source: &'a str,
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
    comments: SingleThreadedComments,
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
        if next > MAX_SOURCE_NESTING_DEPTH {
            return Err(source_nesting_diagnostic(span));
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
                    swc::ModuleItem::Stmt(stmt) => self.convert_stmt(stmt),
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
                .map(|stmt| self.convert_stmt(stmt))
                .collect()
        })
    }

    fn convert_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        let label = self.statement_label(stmt.span().lo)?;
        let converted = self.convert_unlabeled_stmt(stmt)?;
        let labeled = match label {
            Some(label) => Stmt::Labeled {
                label,
                stmt: Box::new(converted),
            },
            None => converted,
        };
        Ok(Stmt::Spanned(source_span(stmt.span()), Box::new(labeled)))
    }

    /// The label named by the statement's own leading doc comments.
    ///
    /// Every other comment is trivia. Two labels on one statement is the one
    /// refusal: the author wrote two names for one node and no rule for
    /// picking between them would be the one they meant.
    fn statement_label(&self, start: BytePos) -> Result<Option<NodeLabel>, Diagnostic> {
        let Some(comments) = self.comments.get_leading(start) else {
            return Ok(None);
        };
        let mut labels = comments
            .iter()
            .filter(|comment| comment.kind == CommentKind::Block)
            .filter(|comment| is_label_comment(&comment.text));
        let Some(first) = labels.next() else {
            return Ok(None);
        };
        if let Some(second) = labels.next() {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateNodeLabel,
                "this statement already carries an `@label` doc comment",
                Some(source_span(second.span)),
            ));
        }
        Ok(parse_label_comment(&first.text))
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

    fn convert_unlabeled_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
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
            swc::Stmt::If(stmt) => Stmt::If {
                test: self.convert_expr(&stmt.test)?,
                consequent: Box::new(self.convert_body_stmt(&stmt.cons)?),
                alternate: stmt
                    .alt
                    .as_deref()
                    .map(|stmt| self.convert_body_stmt(stmt).map(Box::new))
                    .transpose()?,
            },
            swc::Stmt::While(stmt) => Stmt::While {
                test: self.convert_expr(&stmt.test)?,
                body: Box::new(self.convert_body_stmt(&stmt.body)?),
            },
            swc::Stmt::Break(_) => Stmt::Break,
            swc::Stmt::Continue(_) => Stmt::Continue,
            swc::Stmt::Throw(stmt) => Stmt::Throw(self.convert_expr(&stmt.arg)?),
            swc::Stmt::Try(stmt) => Stmt::Try {
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
            },
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
            swc::Decl::TsInterface(_) | swc::Decl::TsTypeAlias(_) => Ok(Stmt::Empty),
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
            swc::ForHead::Pat(pattern) => Ok((self.convert_pattern(pattern)?, None)),
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
            swc::Pat::Ident(name) => Pattern::Ident(
                self.identifier(&name.id)?,
                name.type_ann
                    .as_ref()
                    .map(|annotation| types::convert_type(&annotation.type_ann)),
            ),
            swc::Pat::Expr(expr) => match self.convert_expr(expr)? {
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
            swc::PropName::BigInt(_) => {
                return Err(reject(
                    DiagnosticCode::BigIntUnsupported,
                    "BigInt object keys",
                    Some(source_span(name.span())),
                ));
            }
        })
    }

    fn convert_function(
        &self,
        name: Option<String>,
        function: &swc::Function,
    ) -> Result<Function, Diagnostic> {
        let span = Some(source_span(function.span));
        if function.is_generator {
            return Err(reject(
                DiagnosticCode::GeneratorUnsupported,
                "generators",
                span,
            ));
        }
        if !function.decorators.is_empty() {
            return Err(reject(
                DiagnosticCode::DecoratorUnsupported,
                "decorators",
                span,
            ));
        }
        let body = function.body.as_ref().ok_or_else(|| {
            reject_refusal(
                DiagnosticCode::UnsupportedStatement,
                "function declarations without bodies",
                span,
            )
        })?;
        let (params, body) = self.in_function(function.is_async, || {
            let params = function
                .params
                .iter()
                .enumerate()
                .filter(|(index, param)| !self.is_this_parameter(*index, &param.pat))
                .map(|(_, param)| self.convert_pattern(&param.pat))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((params, self.convert_statements(&body.stmts)?))
        })?;
        Ok(Function {
            name,
            params,
            body: FunctionBody::Block(body),
            is_async: function.is_async,
            is_arrow: false,
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
                    crate::regex::validate_literal(&pattern, &flags, span)?;
                    Expr::RegExp { pattern, flags }
                }
                swc::Lit::BigInt(_) => {
                    return Err(reject(
                        DiagnosticCode::BigIntUnsupported,
                        "BigInt literals",
                        span,
                    ));
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
            swc::Expr::Arrow(function) => {
                if function.is_generator {
                    return Err(reject(
                        DiagnosticCode::GeneratorUnsupported,
                        "generators",
                        span,
                    ));
                }
                // An async arrow's parameters reserve `await`; a plain
                // arrow's inherit the enclosing rule (§15.3, §15.9).
                let parameter_rule = self.in_async_function.get() || function.is_async;
                let params = self.in_function(parameter_rule, || {
                    self.in_arrow_parameters(|| {
                        function
                            .params
                            .iter()
                            .enumerate()
                            .filter(|(index, param)| !self.is_this_parameter(*index, param))
                            .map(|(_, param)| self.convert_pattern(param))
                            .collect::<Result<Vec<_>, _>>()
                    })
                })?;
                let body = self.in_function(function.is_async, || {
                    Ok(match function.body.as_ref() {
                        swc::BlockStmtOrExpr::BlockStmt(block) => {
                            FunctionBody::Block(self.convert_statements(&block.stmts)?)
                        }
                        swc::BlockStmtOrExpr::Expr(expr) => {
                            FunctionBody::Expression(Box::new(self.convert_expr(expr)?))
                        }
                    })
                })?;
                Expr::Function(Function {
                    name: None,
                    params,
                    body,
                    is_async: function.is_async,
                    is_arrow: true,
                })
            }
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
            swc::Expr::Assign(expr) => Expr::Assign {
                target: self.convert_assign_target(&expr.left)?,
                op: convert_assign_op(expr.op),
                value: Box::new(self.convert_expr(&expr.right)?),
            },
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
            swc::Expr::TsTypeAssertion(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsConstAssertion(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsNonNull(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsAs(expr) => self.convert_expr(&expr.expr)?,
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
            swc::Expr::TaggedTpl(_) => {
                return Err(reject(
                    DiagnosticCode::TaggedTemplateUnsupported,
                    "tagged templates",
                    span,
                ));
            }
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
                Expr::Binary { left, op, right }
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
            return Err(reject(
                DiagnosticCode::PrototypeMutationUnsupported,
                "prototype access",
                Some(source_span(member.span)),
            ));
        }
        let property = match &member.prop {
            swc::MemberProp::Ident(name) => {
                if is_prototype_chain_property(name.sym.as_ref())
                    && !(name.sym.as_ref() == "prototype"
                        && matches!(&member.obj.as_ref(), swc::Expr::Ident(owner) if lashlang::is_javascript_builtin_global(owner.sym.as_ref())))
                {
                    return Err(prototype_access_rejection(member.span));
                }
                MemberProperty::Field(self.identifier_name(name)?)
            }
            swc::MemberProp::Computed(property) => {
                if let swc::Expr::Lit(swc::Lit::Str(name)) = property.expr.as_ref()
                    && is_prototype_chain_property(&name.value.to_string_lossy())
                {
                    return Err(prototype_access_rejection(member.span));
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

    fn convert_update_target(&self, expr: &swc::Expr) -> Result<AssignTarget, Diagnostic> {
        if let Some(diagnostic) = expr.as_member().and_then(builtin_prototype_mutation) {
            return Err(diagnostic);
        }
        match self.convert_expr(expr)? {
            Expr::Ident(name, _) => Ok(AssignTarget::Ident(name)),
            Expr::Member {
                object, property, ..
            } => Ok(AssignTarget::Member { object, property }),
            _ => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "Unsupported: update on a non-assignment target. Assign the expression to a variable first.",
                Some(source_span(expr.span())),
            )),
        }
    }

    fn convert_assign_target(
        &self,
        target: &swc::AssignTarget,
    ) -> Result<AssignTarget, Diagnostic> {
        match target {
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Ident(name)) => {
                Ok(AssignTarget::Ident(self.identifier(&name.id)?))
            }
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Member(member)) => {
                if let Some(diagnostic) = builtin_prototype_mutation(member) {
                    return Err(diagnostic);
                }
                let Expr::Member {
                    object, property, ..
                } = self.convert_member(member)?
                else {
                    unreachable!()
                };
                Ok(AssignTarget::Member { object, property })
            }
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Paren(paren)) => {
                match self.convert_expr(&paren.expr)? {
                    Expr::Ident(name, _) => Ok(AssignTarget::ParenIdent(name)),
                    Expr::Member {
                        object, property, ..
                    } => Ok(AssignTarget::Member { object, property }),
                    _ => Err(Diagnostic::refusal(
                        DiagnosticCode::UnsupportedExpression,
                        "Unsupported: this assignment target. Assign to an identifier, member, index, or destructuring pattern.",
                        Some(source_span(paren.expr.span())),
                    )),
                }
            }
            swc::AssignTarget::Pat(pattern) => {
                let pattern: swc::Pat = pattern.clone().into();
                Ok(AssignTarget::Pattern(Box::new(
                    self.convert_pattern(&pattern)?,
                )))
            }
            _ => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "Unsupported: this assignment target. Assign to an identifier, member, index, or destructuring pattern.",
                Some(source_span(target.span())),
            )),
        }
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
