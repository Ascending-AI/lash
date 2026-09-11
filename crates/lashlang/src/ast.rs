use compact_str::CompactString;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::lexer::Span;

pub type AstString = CompactString;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declarations: Vec<Declaration>,
    pub main: Expr,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declaration_spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expression_spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expression_source_spans: Vec<ExpressionSourceSpan>,
}

/// The nesting limit an AST must satisfy, whether it came from source or was
/// built directly.
///
/// `Try`, `Throw`, `Function` and the other AST-only shapes have no source
/// grammar to bound them, so every AST-construction entry point applies this
/// explicitly and the whole pipeline (link, compile, execute) keeps the 2 MiB
/// stack contract the parsed path already had.
///
/// The value is derived from measurement, not arithmetic. On a 2 MiB thread the
/// full link/compile/execute pipeline first aborts at an AST depth of 74 for the
/// most expensive per-level variant (nested `try`/`catch`/`finally`) and at 77
/// for the cheapest block-bodied one, so 64 keeps ten levels of margin under
/// the tighter cliff. `tests/stack_budget.rs` pins that margin with the most
/// expensive variant at exactly this depth.
///
/// The parser's own cap is set so that no program it accepts can exceed this:
/// a syntactic level is not an AST level, and block-bodied constructs
/// (`if`/`while`/`for`) build an `Expr::Block` inside them and cost two.
/// `tests/nesting_cap.rs` is what pins that relation — walking a family of
/// parsed shapes to the parser's refusal point and requiring every accepted
/// program to pass this check and to link — rather than a constant comparison,
/// which cannot see the per-level cost difference.
pub const MAX_AST_NESTING_DEPTH: usize = 64;

/// An AST that nests deeper than [`MAX_AST_NESTING_DEPTH`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expression nesting too deep (limit {limit}); flatten the program")]
pub struct NestingTooDeep {
    pub limit: usize,
}

/// An AST that cannot be compiled as written.
///
/// AST-only nodes have no parser to reject them out of place, so the checks a
/// parsed program gets for free are applied at the AST-construction entry
/// points instead.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidAst {
    /// The tree nests deeper than [`MAX_AST_NESTING_DEPTH`].
    #[error(transparent)]
    NestingTooDeep {
        #[from]
        source: NestingTooDeep,
    },
    /// A `break` or `continue` appears with no enclosing loop in the same
    /// function body. The parser rejects this in source; a host-built tree has
    /// to be told.
    #[error("`{keyword}` is used outside a loop")]
    LoopControlOutsideLoop { keyword: &'static str },
    /// A JavaScript-style `return` appears outside a function body.
    #[error("`return` is used outside a function")]
    ReturnOutsideFunction,
    #[error(transparent)]
    InvalidProcessSignature {
        #[from]
        source: ProcessSignatureError,
    },
    /// A host-only unknown callable shape was placed in program-owned IR.
    #[error("process type with unknown signature is only valid in host schemas")]
    UnknownProcessSignature,
}

/// Rejects an AST the compiler cannot lower as written.
///
/// This is the AST-construction counterpart of the parser's own validation: it
/// runs before any recursive walk, so an over-deep or ill-formed tree becomes a
/// typed error rather than a stack overflow or a panic deeper in the compiler.
pub fn validate_ast(program: &Program) -> Result<(), InvalidAst> {
    check_ast_nesting_depth(program)?;
    check_program_process_types(program)?;
    check_loop_control(&program.main)?;
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => check_loop_control(&process.body)?,
            // A declared function compiles to a real call frame, so `return`
            // is legal in its body while `break`/`continue` still need a loop.
            Declaration::Function(function) => check_function_loop_control(&function.body)?,
            Declaration::Type(_) => {}
        }
    }
    Ok(())
}

fn check_program_process_types(program: &Program) -> Result<(), InvalidAst> {
    for declaration in &program.declarations {
        match declaration {
            Declaration::Type(declaration) => check_process_type(&declaration.ty)?,
            Declaration::Process(process) => {
                ProcessSignature::try_new(process.params.clone(), TypeExpr::Any)?;
                for param in &process.params {
                    check_process_type(&param.ty)?;
                }
                for signal in &process.signals {
                    check_process_type(&signal.ty)?;
                }
                if let Some(return_ty) = &process.return_ty {
                    check_process_type(return_ty)?;
                }
                check_expr_process_types(&process.body)?;
            }
            Declaration::Function(function) => {
                for param in &function.params {
                    check_process_type(&param.ty)?;
                }
                check_process_type(&function.return_ty)?;
                check_expr_process_types(&function.body)?;
            }
        }
    }
    check_expr_process_types(&program.main)
}

fn check_expr_process_types(expr: &Expr) -> Result<(), InvalidAst> {
    if let Expr::TypeLiteral(ty) = expr {
        check_process_type(ty)?;
    }
    for child in expr.children() {
        check_expr_process_types(child)?;
    }
    Ok(())
}

fn check_process_type(ty: &TypeExpr) -> Result<(), InvalidAst> {
    match ty {
        TypeExpr::List(item) | TypeExpr::TriggerHandle(item) => check_process_type(item),
        TypeExpr::Object(fields) => {
            for field in fields {
                check_process_type(&field.ty)?;
            }
            Ok(())
        }
        TypeExpr::Union(items) => {
            for item in items {
                check_process_type(item)?;
            }
            Ok(())
        }
        TypeExpr::Process(process) => {
            let Some(signature) = process.as_signature() else {
                return Err(InvalidAst::UnknownProcessSignature);
            };
            for param in signature.params() {
                check_process_type(&param.ty)?;
            }
            check_process_type(signature.output())
        }
        TypeExpr::Any
        | TypeExpr::Str
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Bool
        | TypeExpr::Dict
        | TypeExpr::Null
        | TypeExpr::Enum(_)
        | TypeExpr::Ref(_) => Ok(()),
    }
}

/// Walks one function body, tracking whether a loop encloses each node.
///
/// Loop bodies are the only place `break` and `continue` are legal, and a
/// nested `Expr::Function` starts a fresh body: the compiler saves and restores
/// its loop contexts across one, so an enclosing loop outside the function does
/// not reach in.
fn check_loop_control(root: &Expr) -> Result<(), InvalidAst> {
    check_loop_control_inner(root, false)
}

/// Walks a declared function's body, which is itself a function body.
fn check_function_loop_control(root: &Expr) -> Result<(), InvalidAst> {
    check_loop_control_inner(root, true)
}

fn check_loop_control_inner(root: &Expr, in_function: bool) -> Result<(), InvalidAst> {
    let mut pending: Vec<(&Expr, bool, bool)> = vec![(root, false, in_function)];
    while let Some((expr, in_loop, in_function)) = pending.pop() {
        match expr {
            Expr::Break if !in_loop => {
                return Err(InvalidAst::LoopControlOutsideLoop { keyword: "break" });
            }
            Expr::Continue if !in_loop => {
                return Err(InvalidAst::LoopControlOutsideLoop {
                    keyword: "continue",
                });
            }
            Expr::Return(_) if !in_function => return Err(InvalidAst::ReturnOutsideFunction),
            Expr::For { iterable, body, .. } => {
                pending.push((iterable, in_loop, in_function));
                pending.push((body, true, in_function));
                continue;
            }
            Expr::While { condition, body } => {
                pending.push((condition, in_loop, in_function));
                pending.push((body, true, in_function));
                continue;
            }
            Expr::Function(function) => {
                pending.push((&function.body, false, true));
                continue;
            }
            _ => {}
        }
        for child in expr.children() {
            pending.push((child, in_loop, in_function));
        }
    }
    Ok(())
}

/// Rejects an AST whose expression nesting exceeds [`MAX_AST_NESTING_DEPTH`].
///
/// The walk is iterative on purpose: a recursive measurement would overflow on
/// exactly the inputs this exists to refuse. It descends through
/// [`Expr::children`], which is exhaustive over the variants, so a new AST node
/// is covered the moment it is added.
pub fn check_ast_nesting_depth(program: &Program) -> Result<(), NestingTooDeep> {
    let mut pending: Vec<(&Expr, usize)> = Vec::new();
    pending.push((&program.main, 1));
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => pending.push((&process.body, 1)),
            Declaration::Function(function) => pending.push((&function.body, 1)),
            Declaration::Type(_) => {}
        }
    }
    while let Some((expr, depth)) = pending.pop() {
        if depth > MAX_AST_NESTING_DEPTH {
            return Err(NestingTooDeep {
                limit: MAX_AST_NESTING_DEPTH,
            });
        }
        for child in expr.children() {
            pending.push((child, depth + 1));
        }
    }
    Ok(())
}

impl Program {
    pub fn block(expressions: Vec<Expr>) -> Self {
        Self {
            declarations: Vec::new(),
            main: Expr::Block(expressions),
            declaration_spans: Vec::new(),
            expression_spans: Vec::new(),
            expression_source_spans: Vec::new(),
        }
    }

    pub(crate) fn module_with_spans(
        declarations: Vec<Declaration>,
        declaration_spans: Vec<Span>,
        expressions: Vec<Expr>,
        expression_spans: Vec<Span>,
        expression_source_spans: Vec<ExpressionSourceSpan>,
    ) -> Self {
        Self {
            declarations,
            main: Expr::Block(expressions),
            declaration_spans,
            expression_spans,
            expression_source_spans,
        }
    }

    pub fn process(&self, name: &str) -> Option<&ProcessDecl> {
        self.declarations
            .iter()
            .find_map(|declaration| match declaration {
                Declaration::Process(process) if process.name.as_str() == name => Some(process),
                _ => None,
            })
    }
}

impl PartialEq for Program {
    fn eq(&self, other: &Self) -> bool {
        self.declarations == other.declarations && self.main == other.main
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionSourceSpan {
    pub path: Vec<u32>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Declaration {
    Type(TypeDecl),
    Process(ProcessDecl),
    Function(FunctionDecl),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypeDecl {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessDecl {
    pub name: AstString,
    pub params: Vec<ProcessParam>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<ProcessSignalDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_ty: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<LabelMetadata>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessParam {
    pub name: AstString,
    pub ty: TypeExpr,
}

/// A user-defined pure synchronous function.
///
/// A function is the language's only reusable *synchronous* abstraction:
/// `process` is durable and asynchronous, so shared pure logic previously had
/// to be inlined at every use. The declaration is deliberately narrower than
/// `process`: parameters and the return type are both mandatory, and the linker
/// rejects every effect inside the body. That ban is what keeps effect identity
/// untouched — every effect stays at a stable top-level syntactic site, so
/// call-site exactly-once identity and continuation snapshots see no new shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub name: AstString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<FunctionParam>,
    pub return_ty: TypeExpr,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionParam {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSignalDecl {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelMetadata {
    pub title: AstString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<AstString>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssignTarget {
    pub root: AstString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<AssignPathStep>,
}

impl AssignTarget {
    pub fn variable(root: AstString) -> Self {
        Self {
            root,
            steps: Vec::new(),
        }
    }

    pub fn is_simple(&self) -> bool {
        self.steps.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AssignPathStep {
    Field(AstString),
    Index(Expr),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Block(Vec<Expr>),
    LabelAnnotated {
        label: LabelMetadata,
        expr: Box<Expr>,
    },
    Null,
    /// The JavaScript `undefined` value. This node is AST-only.
    Undefined,
    Bool(bool),
    Number(f64),
    String(AstString),
    Variable(AstString),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    ListComprehension {
        element: Box<Expr>,
        clauses: Vec<ListComprehensionClause>,
    },
    Record(Vec<(AstString, Expr)>),
    Assign {
        target: AssignTarget,
        expr: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then_block: Box<Expr>,
        else_block: Box<Expr>,
    },
    For {
        binding: AstString,
        iterable: Box<Expr>,
        body: Box<Expr>,
    },
    While {
        condition: Box<Expr>,
        body: Box<Expr>,
    },
    Break,
    Continue,
    StartProcess(ProcessStartExpr),
    ProcessRef {
        process: AstString,
    },
    HostDescriptorConstructor {
        type_name: AstString,
        input: Box<Expr>,
    },
    ResourceRef(ResourceRefExpr),
    ReceiverCall {
        receiver: Box<Expr>,
        operation: AstString,
        args: Vec<Expr>,
    },
    Await(Box<Expr>),
    SleepFor(Box<Expr>),
    SleepUntil(Box<Expr>),
    WaitSignal {
        name: AstString,
    },
    SignalRun {
        run: Box<Expr>,
        name: AstString,
        payload: Box<Expr>,
    },
    ResultUnwrap(Box<Expr>),
    Cancel(Box<Expr>),
    Print(Box<Expr>),
    Yield(Box<Expr>),
    Wake(Box<Expr>),
    Finish(Box<Expr>),
    Fail(Box<Expr>),
    BuiltinCall {
        name: AstString,
        args: Vec<Expr>,
    },
    /// A user-defined function value. This node is AST-only; the parser never
    /// produces it. Captures are copied when the closure is created.
    Function(Box<FunctionExpr>),
    /// Calls a user-defined function value.
    Call {
        function: Box<Expr>,
        args: Vec<Expr>,
    },
    /// Calls a declared [`FunctionDecl`] by name.
    ///
    /// The parser never produces this node: source spells a call to a declared
    /// function exactly like a builtin call, and the linker is the resolver
    /// that rewrites the `BuiltinCall` whose name it recognises. Keeping the
    /// resolved shape distinct is what lets the compiler emit a static callee
    /// and lets every later pass tell a pure declared call apart from a
    /// first-class closure call.
    FunctionCall {
        function: AstString,
        args: Vec<Expr>,
    },
    /// AST-only map intrinsic used to exercise builtin-to-VM callbacks.
    Map {
        items: Box<Expr>,
        function: Box<Expr>,
    },
    /// AST-only structured exception scope. The parser intentionally has no
    /// production for this node; dialects construct it directly.
    Try(Box<TryExpr>),
    /// AST-only explicit throw. The thrown value is transferred unchanged.
    Throw(Box<Expr>),
    /// AST-only JavaScript function return. The compiler runs every enclosing
    /// `finally` before returning from the current function.
    Return(Box<Expr>),
    Field {
        target: Box<Expr>,
        field: AstString,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    /// An ECMA-262 unary operation whose coercion differs from Lashlang.
    JavaScriptUnary {
        op: JavaScriptUnaryOp,
        expr: Box<Expr>,
    },
    /// An eager ECMA-262 binary operation whose coercion differs from Lashlang.
    JavaScriptBinary {
        left: Box<Expr>,
        op: JavaScriptBinaryOp,
        right: Box<Expr>,
    },
    /// A short-circuiting ECMA-262 logical operation that returns an operand.
    JavaScriptLogical {
        left: Box<Expr>,
        op: JavaScriptLogicalOp,
        right: Box<Expr>,
    },
    TypeLiteral(Box<TypeExpr>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionExpr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<AstString>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TryExpr {
    pub body: Box<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch: Option<CatchClause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finally: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatchClause {
    pub binding: AstString,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ListComprehensionClause {
    For { binding: AstString, iterable: Expr },
    If { condition: Expr },
}

impl Expr {
    /// Yields every direct child expression of `self` in evaluation order.
    ///
    /// This is the single structural-traversal primitive: any pass that only
    /// needs to recurse into the sub-expressions of a node (without caring
    /// about the node's own kind) can fold over `children()` instead of
    /// re-spelling the full `match`. Leaf nodes (`Null`, `Bool`, `Number`,
    /// `String`, `Variable`, `Break`, `Continue`, `WaitSignal`,
    /// `ResourceRef`, `ProcessRef`, `HostDescriptorConstructor` metadata, and
    /// `TypeLiteral`) yield nothing.
    ///
    /// `Assign` includes any dynamic index expressions in its `target` path
    /// (in path order) before the assigned value, matching the order in which
    /// the compiler and linker visit them.
    pub fn children(&self) -> ExprChildren<'_> {
        let mut buffer = SmallExprVec::new();
        match self {
            Expr::Null
            | Expr::Undefined
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::Break
            | Expr::Continue
            | Expr::WaitSignal { .. }
            | Expr::ProcessRef { .. }
            | Expr::ResourceRef(_)
            | Expr::TypeLiteral(_) => {}
            Expr::Block(expressions) | Expr::Tuple(expressions) | Expr::List(expressions) => {
                buffer.extend(expressions.iter());
            }
            Expr::ListComprehension { element, clauses } => {
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { iterable, .. } => buffer.push(iterable),
                        ListComprehensionClause::If { condition } => buffer.push(condition),
                    }
                }
                buffer.push(element);
            }
            Expr::LabelAnnotated { expr, .. } => buffer.push(expr),
            Expr::Record(entries) => buffer.extend(entries.iter().map(|(_, value)| value)),
            Expr::Assign { target, expr } => {
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        buffer.push(index);
                    }
                }
                buffer.push(expr);
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                buffer.push(condition);
                buffer.push(then_block);
                buffer.push(else_block);
            }
            Expr::For { iterable, body, .. } => {
                buffer.push(iterable);
                buffer.push(body);
            }
            Expr::While { condition, body } => {
                buffer.push(condition);
                buffer.push(body);
            }
            Expr::StartProcess(start) => buffer.extend(start.args.iter().map(|(_, value)| value)),
            Expr::HostDescriptorConstructor { input, .. } => buffer.push(input),
            Expr::ReceiverCall { receiver, args, .. } => {
                buffer.push(receiver);
                buffer.extend(args.iter());
            }
            Expr::SignalRun { run, payload, .. } => {
                buffer.push(run);
                buffer.push(payload);
            }
            Expr::Await(expr)
            | Expr::SleepFor(expr)
            | Expr::SleepUntil(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Cancel(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Wake(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. }
            | Expr::JavaScriptUnary { expr, .. }
            | Expr::Return(expr) => buffer.push(expr),
            Expr::Finish(expr) => buffer.push(expr),
            Expr::BuiltinCall { args, .. } | Expr::FunctionCall { args, .. } => {
                buffer.extend(args.iter())
            }
            Expr::Function(function) => buffer.push(&function.body),
            Expr::Call { function, args } => {
                buffer.push(function);
                buffer.extend(args.iter());
            }
            Expr::Map { items, function } => {
                buffer.push(items);
                buffer.push(function);
            }
            Expr::Try(scope) => {
                buffer.push(&scope.body);
                if let Some(catch) = &scope.catch {
                    buffer.push(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    buffer.push(finally);
                }
            }
            Expr::Throw(value) => buffer.push(value),
            Expr::Field { target, .. } => buffer.push(target),
            Expr::Index { target, index } => {
                buffer.push(target);
                buffer.push(index);
            }
            Expr::Binary { left, right, .. }
            | Expr::JavaScriptBinary { left, right, .. }
            | Expr::JavaScriptLogical { left, right, .. } => {
                buffer.push(left);
                buffer.push(right);
            }
        }
        ExprChildren {
            buffer,
            position: 0,
        }
    }
}

type SmallExprVec<'expr> = smallvec::SmallVec<[&'expr Expr; 3]>;

/// Iterator over the direct child expressions yielded by [`Expr::children`].
pub struct ExprChildren<'expr> {
    buffer: SmallExprVec<'expr>,
    position: usize,
}

impl<'expr> Iterator for ExprChildren<'expr> {
    type Item = &'expr Expr;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.buffer.get(self.position).copied();
        if item.is_some() {
            self.position += 1;
        }
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.buffer.len() - self.position;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ExprChildren<'_> {}

pub trait ExprVisitor {
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
}

pub fn walk_expr<V>(visitor: &mut V, expr: &Expr)
where
    V: ExprVisitor + ?Sized,
{
    for child in expr.children() {
        visitor.visit_expr(child);
    }
}

pub trait ExprFolder {
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        fold_expr_children(self, expr)
    }
}

pub fn fold_expr_children<F>(folder: &mut F, expr: Expr) -> Expr
where
    F: ExprFolder + ?Sized,
{
    match expr {
        Expr::Block(expressions) => Expr::Block(
            expressions
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::LabelAnnotated { label, expr } => Expr::LabelAnnotated {
            label,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Tuple(items) => Expr::Tuple(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::List(items) => Expr::List(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::ListComprehension { element, clauses } => Expr::ListComprehension {
            element: Box::new(folder.fold_expr(*element)),
            clauses: clauses
                .into_iter()
                .map(|clause| fold_list_comprehension_clause(folder, clause))
                .collect(),
        },
        Expr::Record(entries) => Expr::Record(
            entries
                .into_iter()
                .map(|(name, value)| (name, folder.fold_expr(value)))
                .collect(),
        ),
        Expr::Assign { target, expr } => Expr::Assign {
            target: fold_assign_target(folder, target),
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::If {
            condition,
            then_block,
            else_block,
        } => Expr::If {
            condition: Box::new(folder.fold_expr(*condition)),
            then_block: Box::new(folder.fold_expr(*then_block)),
            else_block: Box::new(folder.fold_expr(*else_block)),
        },
        Expr::For {
            binding,
            iterable,
            body,
        } => Expr::For {
            binding,
            iterable: Box::new(folder.fold_expr(*iterable)),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::While { condition, body } => Expr::While {
            condition: Box::new(folder.fold_expr(*condition)),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::StartProcess(mut start) => {
            start.args = start
                .args
                .into_iter()
                .map(|(name, value)| (name, folder.fold_expr(value)))
                .collect();
            Expr::StartProcess(start)
        }
        Expr::ProcessRef { process } => Expr::ProcessRef { process },
        Expr::HostDescriptorConstructor { type_name, input } => Expr::HostDescriptorConstructor {
            type_name,
            input: Box::new(folder.fold_expr(*input)),
        },
        Expr::ReceiverCall {
            receiver,
            operation,
            args,
        } => Expr::ReceiverCall {
            receiver: Box::new(folder.fold_expr(*receiver)),
            operation,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Await(expr) => Expr::Await(Box::new(folder.fold_expr(*expr))),
        Expr::SleepFor(expr) => Expr::SleepFor(Box::new(folder.fold_expr(*expr))),
        Expr::SleepUntil(expr) => Expr::SleepUntil(Box::new(folder.fold_expr(*expr))),
        Expr::SignalRun { run, name, payload } => Expr::SignalRun {
            run: Box::new(folder.fold_expr(*run)),
            name,
            payload: Box::new(folder.fold_expr(*payload)),
        },
        Expr::ResultUnwrap(expr) => Expr::ResultUnwrap(Box::new(folder.fold_expr(*expr))),
        Expr::Cancel(expr) => Expr::Cancel(Box::new(folder.fold_expr(*expr))),
        Expr::Print(expr) => Expr::Print(Box::new(folder.fold_expr(*expr))),
        Expr::Yield(expr) => Expr::Yield(Box::new(folder.fold_expr(*expr))),
        Expr::Wake(expr) => Expr::Wake(Box::new(folder.fold_expr(*expr))),
        Expr::Finish(expr) => Expr::Finish(Box::new(folder.fold_expr(*expr))),
        Expr::Fail(expr) => Expr::Fail(Box::new(folder.fold_expr(*expr))),
        Expr::BuiltinCall { name, args } => Expr::BuiltinCall {
            name,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::FunctionCall { function, args } => Expr::FunctionCall {
            function,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Function(function) => Expr::Function(Box::new(FunctionExpr {
            name: function.name,
            params: function.params,
            captures: function.captures,
            body: Box::new(folder.fold_expr(*function.body)),
        })),
        Expr::Call { function, args } => Expr::Call {
            function: Box::new(folder.fold_expr(*function)),
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Map { items, function } => Expr::Map {
            items: Box::new(folder.fold_expr(*items)),
            function: Box::new(folder.fold_expr(*function)),
        },
        Expr::Try(scope) => Expr::Try(Box::new(TryExpr {
            body: Box::new(folder.fold_expr(*scope.body)),
            catch: scope.catch.map(|catch| CatchClause {
                binding: catch.binding,
                body: Box::new(folder.fold_expr(*catch.body)),
            }),
            finally: scope
                .finally
                .map(|finally| Box::new(folder.fold_expr(*finally))),
        })),
        Expr::Throw(value) => Expr::Throw(Box::new(folder.fold_expr(*value))),
        Expr::Return(value) => Expr::Return(Box::new(folder.fold_expr(*value))),
        Expr::Field { target, field } => Expr::Field {
            target: Box::new(folder.fold_expr(*target)),
            field,
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(folder.fold_expr(*target)),
            index: Box::new(folder.fold_expr(*index)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Binary { left, op, right } => Expr::Binary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        Expr::JavaScriptUnary { op, expr } => Expr::JavaScriptUnary {
            op,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::JavaScriptBinary { left, op, right } => Expr::JavaScriptBinary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        Expr::JavaScriptLogical { left, op, right } => Expr::JavaScriptLogical {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        leaf @ (Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::Break
        | Expr::Continue
        | Expr::ResourceRef(_)
        | Expr::WaitSignal { .. }
        | Expr::TypeLiteral(_)) => leaf,
    }
}

fn fold_list_comprehension_clause<F>(
    folder: &mut F,
    clause: ListComprehensionClause,
) -> ListComprehensionClause
where
    F: ExprFolder + ?Sized,
{
    match clause {
        ListComprehensionClause::For { binding, iterable } => ListComprehensionClause::For {
            binding,
            iterable: folder.fold_expr(iterable),
        },
        ListComprehensionClause::If { condition } => ListComprehensionClause::If {
            condition: folder.fold_expr(condition),
        },
    }
}

fn fold_assign_target<F>(folder: &mut F, target: AssignTarget) -> AssignTarget
where
    F: ExprFolder + ?Sized,
{
    AssignTarget {
        root: target.root,
        steps: target
            .steps
            .into_iter()
            .map(|step| match step {
                AssignPathStep::Field(field) => AssignPathStep::Field(field),
                AssignPathStep::Index(index) => AssignPathStep::Index(folder.fold_expr(index)),
            })
            .collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeExpr {
    Any,
    Str,
    Int,
    Float,
    Bool,
    Dict,
    /// The literal `null` type; usually only useful as part of a
    /// `Union` (e.g. `str | null` for a nullable field).
    Null,
    Enum(Vec<AstString>),
    List(Box<TypeExpr>),
    Object(Vec<TypeField>),
    Ref(AstString),
    Process(ProcessType),
    TriggerHandle(Box<TypeExpr>),
    /// Union of alternative type shapes, e.g. `str | int | null`.
    /// Always has two or more variants; single-variant parses collapse
    /// to the underlying `TypeExpr` in the parser.
    Union(Vec<TypeExpr>),
}

/// A checked, ordered process-call signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSignature {
    params: Vec<ProcessParam>,
    output: Box<TypeExpr>,
}

impl ProcessSignature {
    /// Builds a signature after validating source-level parameter names and uniqueness.
    pub fn try_new(
        params: Vec<ProcessParam>,
        output: TypeExpr,
    ) -> Result<Self, ProcessSignatureError> {
        let mut names = std::collections::BTreeSet::new();
        for param in &params {
            if !crate::parser::is_source_identifier(
                param.name.as_str(),
                crate::parser::IdentifierPosition::Identifier,
            ) {
                return Err(ProcessSignatureError::InvalidParameterName {
                    name: param.name.to_string(),
                });
            }
            if !names.insert(param.name.as_str()) {
                return Err(ProcessSignatureError::DuplicateParameter {
                    name: param.name.to_string(),
                });
            }
        }
        Ok(Self {
            params,
            output: Box::new(output),
        })
    }

    /// Returns parameters in invocation order.
    pub fn params(&self) -> &[ProcessParam] {
        &self.params
    }

    /// Returns the process result type.
    pub fn output(&self) -> &TypeExpr {
        &self.output
    }

    /// Returns the derived number of invocation parameters.
    pub fn arity(&self) -> usize {
        self.params.len()
    }
}

/// Why an ordered process signature cannot be constructed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessSignatureError {
    #[error("invalid process parameter name `{name}`")]
    InvalidParameterName { name: String },
    #[error("duplicate process parameter `{name}`")]
    DuplicateParameter { name: String },
}

/// A process callable with either an authoritative signature or an honest host-only unknown shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessType(ProcessTypeKind);

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessTypeKind {
    Unknown,
    Known(ProcessSignature),
}

impl ProcessType {
    /// Wraps a checked authoritative process signature.
    pub fn known(signature: ProcessSignature) -> Self {
        Self(ProcessTypeKind::Known(signature))
    }

    /// Describes a process callable whose host schema makes no signature claim.
    pub fn unknown() -> Self {
        Self(ProcessTypeKind::Unknown)
    }

    /// Returns the authoritative signature when one is known.
    pub fn as_signature(&self) -> Option<&ProcessSignature> {
        match &self.0 {
            ProcessTypeKind::Known(signature) => Some(signature),
            ProcessTypeKind::Unknown => None,
        }
    }
}

impl Serialize for ProcessType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            ProcessTypeKind::Unknown => {
                let mut state = serializer.serialize_struct("ProcessType", 1)?;
                state.serialize_field("kind", "unknown")?;
                state.end()
            }
            ProcessTypeKind::Known(signature) => {
                let mut state = serializer.serialize_struct("ProcessType", 3)?;
                state.serialize_field("kind", "known")?;
                state.serialize_field("params", signature.params())?;
                state.serialize_field("output", signature.output())?;
                state.end()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProcessTypeWire {
    Unknown,
    Known {
        params: Vec<ProcessParamWire>,
        output: TypeExpr,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessParamWire {
    name: AstString,
    ty: TypeExpr,
}

impl<'de> Deserialize<'de> for ProcessType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ProcessTypeWire::deserialize(deserializer)? {
            ProcessTypeWire::Unknown => Ok(Self::unknown()),
            ProcessTypeWire::Known { params, output } => ProcessSignature::try_new(
                params
                    .into_iter()
                    .map(|param| ProcessParam {
                        name: param.name,
                        ty: param.ty,
                    })
                    .collect(),
                output,
            )
            .map(Self::known)
            .map_err(serde::de::Error::custom),
        }
    }
}

pub fn format_type_expr(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Any => "any".to_string(),
        TypeExpr::Str => "str".to_string(),
        TypeExpr::Int => "int".to_string(),
        TypeExpr::Float => "float".to_string(),
        TypeExpr::Bool => "bool".to_string(),
        TypeExpr::Dict => "dict".to_string(),
        TypeExpr::Null => "null".to_string(),
        TypeExpr::Enum(values) => format!(
            "enum[{}]",
            values
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        TypeExpr::List(item) => format!("list[{}]", format_type_expr(item)),
        TypeExpr::Object(fields) => {
            let fields = fields
                .iter()
                .map(|field| {
                    let optional = if field.optional { "?" } else { "" };
                    format!(
                        "{}: {}{}",
                        field.name,
                        format_type_expr(&field.ty),
                        optional
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {fields} }}")
        }
        TypeExpr::Ref(name) => name.to_string(),
        TypeExpr::Process(process) => match process.as_signature() {
            Some(signature) => format!(
                "Process<({}), {}>",
                signature
                    .params()
                    .iter()
                    .map(|param| format!("{}: {}", param.name, format_type_expr(&param.ty)))
                    .collect::<Vec<_>>()
                    .join(", "),
                format_type_expr(signature.output())
            ),
            None => "Process".to_string(),
        },
        TypeExpr::TriggerHandle(event) => {
            format!("TriggerHandle<{}>", format_type_expr(event))
        }
        TypeExpr::Union(items) => items
            .iter()
            .map(format_type_expr)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_type_expr(self))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeField {
    pub name: AstString,
    pub ty: TypeExpr,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessStartExpr {
    pub process: AstString,
    pub args: Vec<(AstString, Expr)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRefExpr {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<AstString>,
    pub resource_type: AstString,
    pub alias: AstString,
}

impl ResourceRefExpr {
    pub fn unresolved(path: Vec<AstString>) -> Self {
        Self {
            path,
            resource_type: AstString::default(),
            alias: AstString::default(),
        }
    }

    pub fn resolved(
        path: Vec<AstString>,
        resource_type: impl Into<AstString>,
        alias: impl Into<AstString>,
    ) -> Self {
        Self {
            path,
            resource_type: resource_type.into(),
            alias: alias.into(),
        }
    }

    pub fn path_string(&self) -> String {
        if self.path.is_empty() {
            format!("{}.{}", self.resource_type, self.alias)
        } else {
            self.path
                .iter()
                .map(AstString::as_str)
                .collect::<Vec<_>>()
                .join(".")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaScriptUnaryOp {
    Plus,
    Negate,
    Not,
    TypeOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaScriptBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    StrictEqual,
    StrictNotEqual,
    LooseEqual,
    LooseNotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaScriptLogicalOp {
    And,
    Or,
    NullishCoalesce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    And,
    Or,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(name: &str, ty: TypeExpr) -> ProcessParam {
        ProcessParam {
            name: name.into(),
            ty,
        }
    }

    #[test]
    fn process_signature_construction_and_wire_shape_are_checked() {
        let process = TypeExpr::Process(ProcessType::known(
            ProcessSignature::try_new(vec![param("message", TypeExpr::Str)], TypeExpr::Bool)
                .expect("valid signature"),
        ));
        assert_eq!(
            serde_json::to_value(&process).unwrap(),
            serde_json::json!({
                "Process": {
                    "kind": "known",
                    "params": [{"name": "message", "ty": "Str"}],
                    "output": "Bool"
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<TypeExpr>(serde_json::to_value(&process).unwrap()).unwrap(),
            process
        );
        assert_eq!(format_type_expr(&process), "Process<(message: str), bool>");

        let unknown = TypeExpr::Process(ProcessType::unknown());
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            serde_json::json!({"Process": {"kind": "unknown"}})
        );
        assert_eq!(format_type_expr(&unknown), "Process");
    }

    #[test]
    fn process_signature_refuses_invalid_names_without_broadening_type_validation() {
        assert!(matches!(
            ProcessSignature::try_new(vec![param("1bad", TypeExpr::Str)], TypeExpr::Bool),
            Err(ProcessSignatureError::InvalidParameterName { .. })
        ));
        assert!(matches!(
            ProcessSignature::try_new(vec![param("if", TypeExpr::Str)], TypeExpr::Bool),
            Err(ProcessSignatureError::InvalidParameterName { .. })
        ));
        assert!(matches!(
            ProcessSignature::try_new(
                vec![param("value", TypeExpr::Str), param("value", TypeExpr::Int)],
                TypeExpr::Bool,
            ),
            Err(ProcessSignatureError::DuplicateParameter { .. })
        ));
        ProcessSignature::try_new(
            vec![param("value", TypeExpr::Enum(Vec::new()))],
            TypeExpr::Union(vec![TypeExpr::Str]),
        )
        .expect("FIG-2879 does not add unrelated TypeExpr restrictions");
    }

    #[test]
    fn process_signature_decode_refuses_missing_duplicate_unknown_and_legacy_fields() {
        for wire in [
            r#"{"Process":{"params":[],"output":"Bool"}}"#,
            r#"{"Process":{"kind":null}}"#,
            r#"{"Process":{"kind":"known","params":null,"output":"Bool"}}"#,
            r#"{"Process":{"kind":"known","params":[],"output":null}}"#,
            r#"{"Process":{"kind":"known","params":[],"params":[],"output":"Bool"}}"#,
            r#"{"Process":{"kind":"known","params":[],"output":"Bool","extra":true}}"#,
            r#"{"Process":{"kind":"known","params":[{"name":"x","ty":"Str","extra":true}],"output":"Bool"}}"#,
            r#"{"Process":{"input":"Str","output":"Bool","input_count":1}}"#,
            r#"{"Process":{"kind":"known","params":[{"name":"x","ty":"Str"},{"name":"x","ty":"Int"}],"output":"Bool"}}"#,
            r#"{"Process":{"kind":"known","params":[{"name":"outer","ty":{"Process":{"kind":"known","params":[{"name":"x","ty":"Str"},{"name":"x","ty":"Int"}],"output":"Bool"}}}],"output":"Bool"}}"#,
        ] {
            assert!(serde_json::from_str::<TypeExpr>(wire).is_err(), "{wire}");
        }
    }

    #[test]
    fn unknown_process_type_is_refused_in_program_ir() {
        let program = Program::block(vec![Expr::TypeLiteral(Box::new(TypeExpr::Process(
            ProcessType::unknown(),
        )))]);
        assert!(matches!(
            validate_ast(&program),
            Err(InvalidAst::UnknownProcessSignature)
        ));
    }

    #[test]
    fn type_expr_formatting_covers_nested_shapes() {
        let ty = TypeExpr::Object(vec![
            TypeField {
                name: "status".into(),
                ty: TypeExpr::Enum(vec!["ok".into(), "err".into()]),
                optional: false,
            },
            TypeField {
                name: "tags".into(),
                ty: TypeExpr::List(Box::new(TypeExpr::Str)),
                optional: true,
            },
            TypeField {
                name: "owner".into(),
                ty: TypeExpr::Ref("User".into()),
                optional: false,
            },
            TypeField {
                name: "value".into(),
                ty: TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::Null]),
                optional: false,
            },
        ]);

        assert_eq!(
            format_type_expr(&ty),
            r#"{ status: enum["ok", "err"], tags: list[str]?, owner: User, value: int | null }"#
        );
        assert_eq!(ty.to_string(), format_type_expr(&ty));
    }

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    fn child_vars(expr: &Expr) -> Vec<String> {
        expr.children()
            .map(|child| match child {
                Expr::Variable(name) => name.to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn children_yields_leaves_as_empty() {
        for leaf in [
            Expr::Null,
            Expr::Bool(true),
            Expr::Number(1.0),
            Expr::String("s".into()),
            var("x"),
            Expr::Break,
            Expr::Continue,
            Expr::WaitSignal {
                name: "ready".into(),
            },
            Expr::TypeLiteral(Box::new(TypeExpr::Str)),
        ] {
            let children: Vec<_> = leaf.children().collect();
            assert!(children.is_empty(), "{leaf:?} should have no children");
        }
    }

    #[test]
    fn children_yields_composite_subexpressions_in_order() {
        let block = Expr::Block(vec![var("a"), var("b"), var("c")]);
        assert_eq!(child_vars(&block), ["a", "b", "c"]);

        let record = Expr::Record(vec![("k1".into(), var("v1")), ("k2".into(), var("v2"))]);
        assert_eq!(child_vars(&record), ["v1", "v2"]);

        let if_expr = Expr::If {
            condition: Box::new(var("cond")),
            then_block: Box::new(var("then")),
            else_block: Box::new(var("else")),
        };
        assert_eq!(child_vars(&if_expr), ["cond", "then", "else"]);

        let while_expr = Expr::While {
            condition: Box::new(var("cond")),
            body: Box::new(var("body")),
        };
        assert_eq!(child_vars(&while_expr), ["cond", "body"]);

        let receiver = Expr::ReceiverCall {
            receiver: Box::new(var("recv")),
            operation: "op".into(),
            args: vec![var("arg0"), var("arg1")],
        };
        assert_eq!(child_vars(&receiver), ["recv", "arg0", "arg1"]);

        let binary = Expr::Binary {
            left: Box::new(var("left")),
            op: BinaryOp::Add,
            right: Box::new(var("right")),
        };
        assert_eq!(child_vars(&binary), ["left", "right"]);
    }

    #[test]
    fn children_yields_assign_index_steps_before_value() {
        let assign = Expr::Assign {
            target: AssignTarget {
                root: "root".into(),
                steps: vec![
                    AssignPathStep::Field("field".into()),
                    AssignPathStep::Index(var("idx")),
                ],
            },
            expr: Box::new(var("value")),
        };
        // Field steps contribute no child expressions; the dynamic index is
        // yielded before the assigned value.
        assert_eq!(child_vars(&assign), ["idx", "value"]);
    }

    #[test]
    fn children_handles_finish() {
        assert_eq!(child_vars(&Expr::Finish(Box::new(var("done")))), ["done"]);
    }

    #[test]
    fn children_size_hint_is_exact() {
        let block = Expr::Block(vec![var("a"), var("b"), var("c"), var("d")]);
        let iter = block.children();
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.size_hint(), (4, Some(4)));
    }

    #[test]
    fn visitor_walks_descendants_through_single_child_boundary() {
        struct VariableCollector(Vec<String>);

        impl ExprVisitor for VariableCollector {
            fn visit_expr(&mut self, expr: &Expr) {
                if let Expr::Variable(name) = expr {
                    self.0.push(name.to_string());
                }
                walk_expr(self, expr);
            }
        }

        let expr = Expr::While {
            condition: Box::new(var("ready")),
            body: Box::new(Expr::Block(vec![
                Expr::Assign {
                    target: AssignTarget {
                        root: "items".into(),
                        steps: vec![AssignPathStep::Index(var("idx"))],
                    },
                    expr: Box::new(var("value")),
                },
                Expr::Finish(Box::new(var("done"))),
            ])),
        };

        let mut collector = VariableCollector(Vec::new());
        collector.visit_expr(&expr);

        assert_eq!(collector.0, ["ready", "idx", "value", "done"]);
    }

    #[test]
    fn folder_reconstructs_owned_expr_trees() {
        struct RenameVariables;

        impl ExprFolder for RenameVariables {
            fn fold_expr(&mut self, expr: Expr) -> Expr {
                match expr {
                    Expr::Variable(name) => Expr::Variable(format!("renamed_{name}").into()),
                    other => fold_expr_children(self, other),
                }
            }
        }

        let expr = Expr::Assign {
            target: AssignTarget {
                root: "items".into(),
                steps: vec![AssignPathStep::Index(var("idx"))],
            },
            expr: Box::new(Expr::List(vec![var("first"), var("second")])),
        };

        let mut folder = RenameVariables;
        let folded = folder.fold_expr(expr);

        let Expr::Assign { target, expr } = folded else {
            panic!("expected assign");
        };
        assert!(matches!(
            target.steps.as_slice(),
            [AssignPathStep::Index(Expr::Variable(name))] if name.as_str() == "renamed_idx"
        ));
        let Expr::List(items) = *expr else {
            panic!("expected list");
        };
        assert_eq!(items, vec![var("renamed_first"), var("renamed_second")]);
    }
}
