use schemars::JsonSchema;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

pub use crate::ast_string::AstString;
use crate::span::Span;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declarations: Vec<Declaration>,
    pub main: Expr,
    /// Source spans for the program's nodes, addressed by [`AstPath`]. A
    /// declaration's own span lives at `AstPath::declaration(i, [])`; absence
    /// is "no span", so no sentinel ever doubles as offset zero.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "span_table"
    )]
    pub spans: BTreeMap<AstPath, Span>,
}

/// Which tree an [`AstPath`] walks down: `Program::main`, or one entry of
/// `Program::declarations`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AstRoot {
    Main,
    Declaration(u32),
}

/// A node's address in a `Program`: the root it hangs from plus the
/// `Expr::children()` index chain that reaches it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AstPath {
    pub root: AstRoot,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<u32>,
}

impl AstPath {
    /// The node `steps` below `Program::main` (`[]` addresses `main` itself).
    pub fn main(steps: impl Into<Vec<u32>>) -> Self {
        Self {
            root: AstRoot::Main,
            steps: steps.into(),
        }
    }

    /// The node `steps` below the body of `Program::declarations[index]`
    /// (`[]` addresses the declaration itself).
    pub fn declaration(index: u32, steps: impl Into<Vec<u32>>) -> Self {
        Self {
            root: AstRoot::Declaration(index),
            steps: steps.into(),
        }
    }

    /// The path of this node's `index`-th `children()` element.
    pub fn child(&self, index: u32) -> Self {
        let mut steps = self.steps.clone();
        steps.push(index);
        Self {
            root: self.root,
            steps,
        }
    }

    /// The flat encoding the lifted-process name hash predates this type on:
    /// `main` paths are the bare steps; declaration paths are prefixed with
    /// `u32::MAX` and the declaration index. Kept for that hash only — a
    /// durable identity input that must not change.
    pub(crate) fn legacy_steps(&self) -> Vec<u32> {
        match self.root {
            AstRoot::Main => self.steps.clone(),
            AstRoot::Declaration(index) => {
                let mut steps = Vec::with_capacity(self.steps.len() + 2);
                steps.push(u32::MAX);
                steps.push(index);
                steps.extend_from_slice(&self.steps);
                steps
            }
        }
    }
}

/// `Program::spans` serializes as a list of entries: a `BTreeMap`'s struct
/// key is not a JSON object key, and `ModuleArtifact` encodes `Program` as
/// JSON. Iteration order is already key order, so the form stays canonical.
mod span_table {
    use super::{AstPath, Span};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    struct SpanEntry {
        path: AstPath,
        span: Span,
    }

    pub(super) fn serialize<S: Serializer>(
        spans: &BTreeMap<AstPath, Span>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(spans.iter().map(|(path, span)| SpanEntry {
            path: path.clone(),
            span: *span,
        }))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<AstPath, Span>, D::Error> {
        Ok(Vec::<SpanEntry>::deserialize(deserializer)?
            .into_iter()
            .map(|entry| (entry.path, entry.span))
            .collect())
    }
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
                ProcessSignature::validate_params(&process.params)?;
                for param in &process.params {
                    check_process_type(&param.ty)?;
                }
                for signal in &process.signals {
                    check_process_type(&signal.ty)?;
                }
                // A process's output is not an authored signature position: the
                // linker infers it from the `finish` types the body reaches, and
                // since ADR 0095 `processes.start` answers the one process type
                // of unknown signature (`{"x-lash":{"kind":"process_unknown"}}`).
                // A start handle may be bound, awaited and finished like any
                // other value, so an unknown signature is legal wherever such a
                // value flows. The refusal stays on what an author *declares*:
                // params, signals, type declarations and type literals.
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

/// Loop bodies are the only place `break` and `continue` are legal, and a
/// nested `Expr::Function` starts a fresh body: the compiler saves and restores
/// its loop contexts across one, so an enclosing loop outside the function does
/// not reach in.
fn check_loop_control(root: &Expr) -> Result<(), InvalidAst> {
    check_loop_control_inner(root, false)
}

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
            spans: BTreeMap::new(),
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Declaration {
    Type(TypeDecl),
    Process(ProcessDecl),
    Function(FunctionDecl),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FunctionDecl {
    pub name: AstString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<FunctionParam>,
    pub return_ty: TypeExpr,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FunctionParam {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessSignalDecl {
    pub name: AstString,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LabelMetadata {
    pub title: AstString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<AstString>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum AssignPathStep {
    Field(AstString),
    Index(Expr),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    ResultUnwrap(Box<Expr>),
    Print(Box<Expr>),
    Yield(Box<Expr>),
    Finish(Box<Expr>),
    Fail(Box<Expr>),
    BuiltinCall {
        name: AstString,
        args: Vec<Expr>,
    },
    /// A user-defined function value. This node is AST-only; the parser never
    /// produces it. Captures are copied when the closure is created.
    Function(Box<FunctionExpr>),
    /// An inline process body written where a `Process`-typed slot is
    /// expected. This node is AST-only and never survives the link: the
    /// linker's expected-type hook lifts it to a hoisted [`ProcessDecl`], and a
    /// literal whose slot is not a process is a type error.
    ProcessLiteral(Box<ProcessLiteralExpr>),
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FunctionExpr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<AstString>,
    pub body: Box<Expr>,
}

/// The prefix on every declaration name the linker derives from a lifted
/// process literal. A dialect that authors process names from source text can
/// never collide with one, because the linker invents these and no authored
/// name can start with it by accident.
pub const LIFTED_PROCESS_NAME_PREFIX: &str = "__process_";

/// The name a body at a given AST path lifts to.
///
/// A digest over the canonical body plus the path, so it is a function of what
/// the body *is* and where it sits — never of link order, span tables, or
/// anything else a re-derivation could reorder. The linker's lift and the
/// workflow lens's literal projection must agree on this spelling.
pub fn lifted_process_identity(body: &Expr, path: &[u32]) -> String {
    let preimage = serde_json::json!({
        "body": body,
        "path": path,
    });
    let digest = lash_sansio::core_support::blake3_domain_hash_hex(
        "lash-lifted-process-name/v1",
        preimage.to_string(),
    );
    format!("{LIFTED_PROCESS_NAME_PREFIX}{digest}")
}

/// The authored shape of an inline process body, as a dialect lowers it.
///
/// `params` carries the parameter names and their declared types, so a
/// TypeScript arrow's annotations reach the lifted declaration's signature
/// instead of widening to `Any`. `body` is the same wrapper a process literal
/// run lowers to: the authored statements inside the process-failure wrapper,
/// with the params passed through by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessLiteralExpr {
    pub params: Vec<ProcessParam>,
    /// Immutable, durably representable cell locals the body reads; each
    /// becomes a hidden start argument carrying the value the variable had
    /// when the process started (FIG-2998).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_args: Vec<ProcessParam>,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TryExpr {
    pub body: Box<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch: Option<CatchClause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finally: Option<Box<Expr>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CatchClause {
    pub binding: AstString,
    pub body: Box<Expr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum ListComprehensionClause {
    For { binding: AstString, iterable: Expr },
    If { condition: Expr },
}

impl Expr {
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
            Expr::HostDescriptorConstructor { input, .. } => buffer.push(input),
            Expr::ReceiverCall { receiver, args, .. } => {
                buffer.push(receiver);
                buffer.extend(args.iter());
            }
            Expr::Await(expr)
            | Expr::SleepFor(expr)
            | Expr::SleepUntil(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. }
            | Expr::JavaScriptUnary { expr, .. }
            | Expr::Return(expr) => buffer.push(expr),
            Expr::Finish(expr) => buffer.push(expr),
            Expr::BuiltinCall { args, .. } | Expr::FunctionCall { args, .. } => {
                buffer.extend(args.iter())
            }
            Expr::Function(function) => buffer.push(&function.body),
            Expr::ProcessLiteral(literal) => buffer.push(&literal.body),
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

    /// The mutable twin of [`Expr::children`]: same nodes, same order. A pass
    /// that rewrites sub-expressions in place — the workflow lens splicing a
    /// rendered process body back into the literal it was lifted from, for
    /// one — walks with this instead of re-spelling the full `match`. The two
    /// walks are pinned to agree by
    /// `children_mut_visits_the_same_nodes_as_children`.
    pub fn children_mut(&mut self) -> ExprChildrenMut<'_> {
        let mut buffer = SmallExprMutVec::new();
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
                buffer.extend(expressions.iter_mut());
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
            Expr::Record(entries) => buffer.extend(entries.iter_mut().map(|(_, value)| value)),
            Expr::Assign { target, expr } => {
                for step in &mut target.steps {
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
            Expr::HostDescriptorConstructor { input, .. } => buffer.push(input),
            Expr::ReceiverCall { receiver, args, .. } => {
                buffer.push(receiver);
                buffer.extend(args.iter_mut());
            }
            Expr::Await(expr)
            | Expr::SleepFor(expr)
            | Expr::SleepUntil(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. }
            | Expr::JavaScriptUnary { expr, .. }
            | Expr::Return(expr) => buffer.push(expr),
            Expr::Finish(expr) => buffer.push(expr),
            Expr::BuiltinCall { args, .. } | Expr::FunctionCall { args, .. } => {
                buffer.extend(args.iter_mut())
            }
            Expr::Function(function) => buffer.push(&mut function.body),
            Expr::ProcessLiteral(literal) => buffer.push(&mut literal.body),
            Expr::Call { function, args } => {
                buffer.push(function);
                buffer.extend(args.iter_mut());
            }
            Expr::Map { items, function } => {
                buffer.push(items);
                buffer.push(function);
            }
            Expr::Try(scope) => {
                buffer.push(&mut scope.body);
                if let Some(catch) = &mut scope.catch {
                    buffer.push(&mut catch.body);
                }
                if let Some(finally) = &mut scope.finally {
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
        ExprChildrenMut {
            inner: buffer.into_iter(),
        }
    }
}

type SmallExprVec<'expr> = smallvec::SmallVec<[&'expr Expr; 8]>;
type SmallExprMutVec<'expr> = smallvec::SmallVec<[&'expr mut Expr; 8]>;

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

/// Iterator over the direct child expressions yielded by [`Expr::children_mut`].
///
/// The buffer is consumed, so each child is handed out exactly once and the
/// mutable borrows it yields can never alias.
pub struct ExprChildrenMut<'expr> {
    inner: smallvec::IntoIter<[&'expr mut Expr; 8]>,
}

impl<'expr> Iterator for ExprChildrenMut<'expr> {
    type Item = &'expr mut Expr;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for ExprChildrenMut<'_> {}

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
        Expr::ResultUnwrap(expr) => Expr::ResultUnwrap(Box::new(folder.fold_expr(*expr))),
        Expr::Print(expr) => Expr::Print(Box::new(folder.fold_expr(*expr))),
        Expr::Yield(expr) => Expr::Yield(Box::new(folder.fold_expr(*expr))),
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
        Expr::ProcessLiteral(literal) => Expr::ProcessLiteral(Box::new(ProcessLiteralExpr {
            params: literal.params,
            hidden_args: literal.hidden_args,
            body: Box::new(folder.fold_expr(*literal.body)),
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

/// A serialized value-type expression.
///
/// Host decoders must refuse unknown variants. `TypeExpr` is decoded only
/// after its graph or facet carrier version is accepted; adding a variant
/// therefore requires the owning carrier version to advance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    Union(UnionMembers),
}

/// The members of a [`TypeExpr::Union`]: two or more by construction.
///
/// A union of one is that member and a union of zero is meaningless, so
/// those states are unrepresentable instead of carried as a degenerate
/// `Union` the artifact encoding would count and write. Build one through
/// [`UnionMembers::new`] when the members are already normalized, or
/// [`UnionMembers::deduplicated`] to flatten nested unions and drop
/// duplicates first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnionMembers(Vec<TypeExpr>);

impl JsonSchema for UnionMembers {
    fn schema_name() -> String {
        "UnionMembers".to_string()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        generator.subschema_for::<Vec<TypeExpr>>()
    }
}

impl UnionMembers {
    pub fn new(members: Vec<TypeExpr>) -> Option<Self> {
        (members.len() >= 2).then_some(Self(members))
    }

    /// Flattens nested unions and drops duplicate members (first-seen
    /// order). `Err` hands back the normalized remainder — zero or one
    /// member — for the caller's collapse rule.
    pub fn deduplicated(members: Vec<TypeExpr>) -> Result<Self, Vec<TypeExpr>> {
        let mut flattened = Vec::new();
        for member in members {
            match member {
                TypeExpr::Union(nested) => flattened.extend(nested),
                member => flattened.push(member),
            }
        }
        let mut unique: Vec<TypeExpr> = Vec::new();
        for member in flattened {
            if !unique.contains(&member) {
                unique.push(member);
            }
        }
        if unique.len() >= 2 {
            Ok(Self(unique))
        } else {
            Err(unique)
        }
    }

    /// The result still holds at least two because the map preserves member count.
    pub fn map(&self, f: impl Fn(&TypeExpr) -> TypeExpr) -> Self {
        Self(self.0.iter().map(f).collect())
    }

    pub fn as_slice(&self) -> &[TypeExpr] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<TypeExpr> {
        self.0
    }
}

impl std::ops::Deref for UnionMembers {
    type Target = [TypeExpr];

    fn deref(&self) -> &[TypeExpr] {
        &self.0
    }
}

impl IntoIterator for UnionMembers {
    type Item = TypeExpr;
    type IntoIter = std::vec::IntoIter<TypeExpr>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a UnionMembers {
    type Item = &'a TypeExpr;
    type IntoIter = std::slice::Iter<'a, TypeExpr>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Members serialize as the same bare sequence `Union(Vec<TypeExpr>)`
/// wrote; decoding re-validates the two-member floor.
impl Serialize for UnionMembers {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UnionMembers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let members = Vec::<TypeExpr>::deserialize(deserializer)?;
        Self::new(members)
            .ok_or_else(|| serde::de::Error::custom("a union type needs at least two members"))
    }
}

impl TypeExpr {
    /// With fewer than two distinct members remaining this collapses — one member to itself,
    /// none to `Null`, the empty union (domains that widen instead, like the JSON-Schema
    /// importer, keep their own policy).
    pub fn union(members: Vec<TypeExpr>) -> TypeExpr {
        match UnionMembers::deduplicated(members) {
            Ok(members) => TypeExpr::Union(members),
            Err(rest) => rest.into_iter().next().unwrap_or(TypeExpr::Null),
        }
    }
}

/// A checked, ordered process-call signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSignature {
    params: Vec<ProcessParam>,
    output: Box<TypeExpr>,
}

impl ProcessSignature {
    pub fn try_new(
        params: Vec<ProcessParam>,
        output: TypeExpr,
    ) -> Result<Self, ProcessSignatureError> {
        Self::validate_params(&params)?;
        Ok(Self {
            params,
            output: Box::new(output),
        })
    }

    /// AST validation only wants the verdict, and cloning the parameter list to
    /// get it is one of the costs publish-time verification pays per process
    /// (FIG-3088). A parameter list is short, so the uniqueness scan is linear
    /// and allocates nothing; it reports the same duplicate - the later of the
    /// two - as the set-based scan did.
    pub fn validate_params(params: &[ProcessParam]) -> Result<(), ProcessSignatureError> {
        for (index, param) in params.iter().enumerate() {
            if !crate::identifier::is_process_parameter_name(param.name.as_str()) {
                return Err(ProcessSignatureError::InvalidParameterName {
                    name: param.name.to_string(),
                });
            }
            if params[..index]
                .iter()
                .any(|earlier| earlier.name == param.name)
            {
                return Err(ProcessSignatureError::DuplicateParameter {
                    name: param.name.to_string(),
                });
            }
        }
        Ok(())
    }

    pub fn params(&self) -> &[ProcessParam] {
        &self.params
    }

    pub fn output(&self) -> &TypeExpr {
        &self.output
    }

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

#[derive(JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "schema-only mirror of ProcessType's custom wire form"
)]
enum ProcessTypeSchema {
    Unknown,
    Known {
        params: Vec<ProcessParam>,
        output: TypeExpr,
    },
}

impl JsonSchema for ProcessType {
    fn schema_name() -> String {
        "ProcessType".to_string()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        ProcessTypeSchema::json_schema(generator)
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeField {
    pub name: AstString,
    pub ty: TypeExpr,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum JavaScriptUnaryOp {
    Plus,
    Negate,
    Not,
    TypeOf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum JavaScriptLogicalOp {
    And,
    Or,
    NullishCoalesce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[path = "ast_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ast_children_tests.rs"]
mod ast_children_tests;
