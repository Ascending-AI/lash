//! Pure constructors for building test programs out of the shared AST.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect, so the crate's own
//! suites no longer author their inputs in the retired surface. They cannot
//! reach the TypeScript front-end either: `lash-typescript` is a *dev*
//! dependency of this crate, and a dev-dependency cycle makes Cargo compile
//! two distinct instances of `lashlang` for the lib-test target, so a
//! `Program` produced by `lash_typescript` is a different type from the one
//! these tests link. Unit tests therefore build the IR directly, which is what
//! they were always really asserting about.
//!
//! Every helper here is a plain constructor. Nothing parses, nothing accepts a
//! string DSL, and nothing supplies a default for a field a test asserts on:
//! each such field is an explicit argument, so a builder can never quietly
//! change what a test pins.

#![allow(dead_code)]

use crate::ast::{
    AssignPathStep, AssignTarget, AstPath, AstString, BinaryOp, CatchClause, Declaration, Expr,
    FunctionDecl, FunctionExpr, FunctionParam, LabelMetadata, ListComprehensionClause, ProcessDecl,
    ProcessParam, ProcessSignalDecl, ProcessSignature, ProcessType, Program, ResourceRefExpr,
    TryExpr, TypeDecl, TypeExpr, TypeField, UnaryOp,
};
use crate::span::Span;

// ---------------------------------------------------------------------------
//  Programs and declarations
// ---------------------------------------------------------------------------

/// A program whose `main` is `expressions`, with no declarations.
pub fn program(expressions: Vec<Expr>) -> Program {
    Program::block(expressions)
}

/// A program with declarations ahead of its top-level expressions.
pub fn module(declarations: Vec<Declaration>, expressions: Vec<Expr>) -> Program {
    Program {
        language: crate::SourceLanguage::ir(),
        declarations,
        main: Expr::Block(expressions),
        private_bindings: Default::default(),
        spans: Default::default(),
    }
}

/// Each entry is `(path, start, end)`: `path` is the child-index route from
/// `program.main` to the expression the span covers, in `Expr::children()`
/// order (`[0]` is the first top-level expression, `[0, 1]` its second child),
/// and the offsets are byte offsets into whatever source text the test renders
/// the diagnostic against.
///
/// FIG-3065: the TypeScript lowerer emits a `Program` with empty span vectors,
/// so nothing in production supplies the offsets the diagnostic renderer needs
/// for its `--> line N, column M` block and caret run. The tests that pin that
/// rendering therefore state the table outright instead of borrowing one from a
/// front-end, which is also what keeps them honest when FIG-3065 is fixed.
pub fn with_source_spans(mut program: Program, entries: &[(&[u32], usize, usize)]) -> Program {
    program
        .spans
        .extend(entries.iter().map(|(path, start, end)| {
            (
                AstPath::main(path.to_vec()),
                Span {
                    start: *start,
                    end: *end,
                },
            )
        }));
    program
}

/// Addressed as `AstPath::main([i])`, in order. See `with_source_spans` for
/// why these tables are stated rather than parsed.
pub fn with_expression_spans(mut program: Program, spans: &[(usize, usize)]) -> Program {
    program
        .spans
        .extend(spans.iter().enumerate().map(|(index, (start, end))| {
            (
                AstPath::main(vec![index as u32]),
                Span {
                    start: *start,
                    end: *end,
                },
            )
        }));
    program
}

/// Addressed as `AstPath::declaration(i, [])`, in declaration order. See
/// `with_source_spans` for why these tables are stated rather than parsed.
pub fn with_declaration_spans(mut program: Program, spans: &[(usize, usize)]) -> Program {
    program
        .spans
        .extend(spans.iter().enumerate().map(|(index, (start, end))| {
            (
                AstPath::declaration(index as u32, Vec::new()),
                Span {
                    start: *start,
                    end: *end,
                },
            )
        }));
    program
}

/// `type <name> = <ty>`
pub fn type_decl(name: &str, ty: TypeExpr) -> Declaration {
    Declaration::Type(TypeDecl {
        name: name.into(),
        ty,
    })
}

/// `process <name>(<params>) { <body> }`, with no signals, return type or label.
pub fn process(name: &str, params: Vec<ProcessParam>, body: Expr) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params,
        signals: Vec::new(),
        return_ty: None,
        label: None,
        origin: crate::ProcessOrigin::Declared,
        body,
    })
}

/// An inline process body where a `Process` slot expects one (FIG-2997).
///
/// `body` is the authored run body, exactly as a dialect hands it to the
/// lift: parameters named by `params`, output read off its `finish` values.
pub fn process_literal(params: Vec<ProcessParam>, body: Expr) -> Expr {
    Expr::ProcessLiteral(Box::new(crate::ProcessLiteralExpr {
        params,
        hidden_args: Vec::new(),
        body: Box::new(body),
    }))
}

/// `process <name>(<params>) -> <return_ty> { <body> }`.
pub fn process_returning(
    name: &str,
    params: Vec<ProcessParam>,
    return_ty: TypeExpr,
    body: Expr,
) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params,
        signals: Vec::new(),
        return_ty: Some(return_ty),
        label: None,
        origin: crate::ProcessOrigin::Declared,
        body,
    })
}

/// `process <name>(<params>) signals { <signals> } { <body> }`.
pub fn process_with_signals(
    name: &str,
    params: Vec<ProcessParam>,
    signals: Vec<ProcessSignalDecl>,
    body: Expr,
) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params,
        signals,
        return_ty: None,
        label: None,
        origin: crate::ProcessOrigin::Declared,
        body,
    })
}

/// A labelled process declaration.
pub fn labelled_process(
    name: &str,
    params: Vec<ProcessParam>,
    label: LabelMetadata,
    body: Expr,
) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params,
        signals: Vec::new(),
        return_ty: None,
        label: Some(label),
        origin: crate::ProcessOrigin::Declared,
        body,
    })
}

pub fn param(name: &str, ty: TypeExpr) -> ProcessParam {
    ProcessParam {
        name: name.into(),
        ty,
    }
}

pub fn signal(name: &str, ty: TypeExpr) -> ProcessSignalDecl {
    ProcessSignalDecl {
        name: name.into(),
        ty,
    }
}

pub fn label(title: &str, description: Option<&str>) -> LabelMetadata {
    LabelMetadata {
        title: title.into(),
        description: description.map(Into::into),
    }
}

/// A declared function: parameters and return type are both mandatory.
pub fn function_decl(
    name: &str,
    params: Vec<FunctionParam>,
    return_ty: TypeExpr,
    body: Expr,
) -> Declaration {
    Declaration::Function(FunctionDecl {
        name: name.into(),
        params,
        return_ty,
        body,
    })
}

pub fn function_param(name: &str, ty: TypeExpr) -> FunctionParam {
    FunctionParam {
        name: name.into(),
        ty,
    }
}

// ---------------------------------------------------------------------------
//  Literals and names
// ---------------------------------------------------------------------------

pub fn null() -> Expr {
    Expr::Null
}

pub fn bool_lit(value: bool) -> Expr {
    Expr::Bool(value)
}

pub fn num(value: f64) -> Expr {
    Expr::Number(value)
}

pub fn string(value: &str) -> Expr {
    Expr::String(value.into())
}

pub fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

pub fn list(items: Vec<Expr>) -> Expr {
    Expr::List(items)
}

pub fn tuple(items: Vec<Expr>) -> Expr {
    Expr::Tuple(items)
}

pub fn record(fields: Vec<(&str, Expr)>) -> Expr {
    Expr::Record(
        fields
            .into_iter()
            .map(|(name, value)| (AstString::from(name), value))
            .collect(),
    )
}

pub fn type_literal(ty: TypeExpr) -> Expr {
    Expr::TypeLiteral(Box::new(ty))
}

/// `Process<(<params>), <output>>` — a known, checked process-callable type.
#[expect(
    clippy::expect_used,
    reason = "test-support builder fixture a #[test] fn calls; the clippy.toml exemptions reach #[test] fns, not this helper, per the message"
)]
pub fn process_type(params: Vec<ProcessParam>, output: TypeExpr) -> TypeExpr {
    TypeExpr::Process(ProcessType::known(
        ProcessSignature::try_new(params, output).expect("process signature should validate"),
    ))
}

pub fn type_field(name: &str, ty: TypeExpr, optional: bool) -> TypeField {
    TypeField {
        name: name.into(),
        ty,
        optional,
    }
}

// ---------------------------------------------------------------------------
//  Structure
// ---------------------------------------------------------------------------

pub fn block(expressions: Vec<Expr>) -> Expr {
    Expr::Block(expressions)
}

/// `<name> = <expr>`
pub fn assign(name: &str, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    }
}

/// `<root><steps> = <expr>`
pub fn assign_path(root: &str, steps: Vec<AssignPathStep>, expr: Expr) -> Expr {
    Expr::Assign {
        target: AssignTarget {
            root: root.into(),
            steps,
        },
        expr: Box::new(expr),
    }
}

pub fn field_step(name: &str) -> AssignPathStep {
    AssignPathStep::Field(name.into())
}

pub fn index_step(index: Expr) -> AssignPathStep {
    AssignPathStep::Index(index)
}

pub fn field(target: Expr, name: &str) -> Expr {
    Expr::Field {
        target: Box::new(target),
        field: name.into(),
    }
}

pub fn index(target: Expr, index: Expr) -> Expr {
    Expr::Index {
        target: Box::new(target),
        index: Box::new(index),
    }
}

pub fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

pub fn unary(op: UnaryOp, expr: Expr) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(expr),
    }
}

pub fn if_else(condition: Expr, then_block: Expr, else_block: Expr) -> Expr {
    Expr::If {
        condition: Box::new(condition),
        then_block: Box::new(then_block),
        else_block: Box::new(else_block),
    }
}

pub fn for_in(binding: &str, iterable: Expr, body: Expr) -> Expr {
    Expr::For {
        binding: binding.into(),
        iterable: Box::new(iterable),
        bind: None,
        body: Box::new(body),
    }
}

/// An iteration whose `bind` runs before each body.
pub fn for_bind(binding: &str, iterable: Expr, bind: Expr, body: Expr) -> Expr {
    Expr::For {
        binding: binding.into(),
        iterable: Box::new(iterable),
        bind: Some(Box::new(bind)),
        body: Box::new(body),
    }
}

/// A structural role around `expr`.
pub fn role(role: crate::StructuralRole, expr: Expr) -> Expr {
    Expr::Role {
        role,
        expr: Box::new(expr),
    }
}

pub fn while_loop(condition: Expr, body: Expr) -> Expr {
    Expr::While {
        condition: Box::new(condition),
        body: Box::new(body),
    }
}

pub fn comprehension(element: Expr, clauses: Vec<ListComprehensionClause>) -> Expr {
    Expr::ListComprehension {
        element: Box::new(element),
        clauses,
    }
}

pub fn comprehension_for(binding: &str, iterable: Expr) -> ListComprehensionClause {
    ListComprehensionClause::For {
        binding: binding.into(),
        iterable,
    }
}

pub fn comprehension_if(condition: Expr) -> ListComprehensionClause {
    ListComprehensionClause::If { condition }
}

pub fn try_expr(body: Expr, catch: Option<CatchClause>, finally: Option<Expr>) -> Expr {
    Expr::Try(Box::new(TryExpr {
        body: Box::new(body),
        catch,
        finally: finally.map(Box::new),
    }))
}

pub fn catch(binding: &str, body: Expr) -> CatchClause {
    CatchClause {
        binding: binding.into(),
        body: Box::new(body),
    }
}

pub fn labelled(label: LabelMetadata, expr: Expr) -> Expr {
    Expr::LabelAnnotated {
        label,
        expr: Box::new(expr),
    }
}

// ---------------------------------------------------------------------------
//  Effects, hosts and processes
// ---------------------------------------------------------------------------

pub fn finish(expr: Expr) -> Expr {
    Expr::Finish(Box::new(expr))
}

pub fn fail(expr: Expr) -> Expr {
    Expr::Fail(Box::new(expr))
}

pub fn print(expr: Expr) -> Expr {
    Expr::Print(Box::new(expr))
}

pub fn yield_expr(expr: Expr) -> Expr {
    Expr::Yield(Box::new(expr))
}

pub fn await_expr(expr: Expr) -> Expr {
    Expr::Await(Box::new(expr))
}

pub fn unwrap(expr: Expr) -> Expr {
    Expr::ResultUnwrap(Box::new(expr))
}

pub fn sleep_for(expr: Expr) -> Expr {
    Expr::SleepFor(Box::new(expr))
}

pub fn sleep_until(expr: Expr) -> Expr {
    Expr::SleepUntil(Box::new(expr))
}

pub fn wait_signal(name: &str) -> Expr {
    Expr::WaitSignal { name: name.into() }
}

pub fn builtin(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

pub fn function_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::FunctionCall {
        function: name.into(),
        args,
    }
}

pub fn closure(name: Option<&str>, params: &[&str], captures: &[&str], body: Expr) -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: name.map(Into::into),
        js_name: None,
        receiver: None,
        params: params.iter().map(|name| AstString::from(*name)).collect(),
        captures: captures.iter().map(|name| AstString::from(*name)).collect(),
        body: Box::new(body),
    }))
}

pub fn call(function: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call {
        function: Box::new(function),
        args,
    }
}

/// An unresolved module reference, e.g. `tools` or `ui.button`.
pub fn resource(path: &[&str]) -> Expr {
    Expr::ResourceRef(ResourceRefExpr::unresolved(
        path.iter().map(|part| AstString::from(*part)).collect(),
    ))
}

/// `<receiver>.<operation>(<args>)`
pub fn receiver_call(receiver: Expr, operation: &str, args: Vec<Expr>) -> Expr {
    Expr::ReceiverCall {
        receiver: Box::new(receiver),
        operation: operation.into(),
        args,
    }
}

/// `await <module>.<operation>(<args>)?` — the shape a tool call takes.
pub fn module_call(path: &[&str], operation: &str, args: Vec<Expr>) -> Expr {
    unwrap(await_expr(receiver_call(resource(path), operation, args)))
}

/// `await processes.start({ definition: <name>, args: { ..args } })?`
///
/// The tool spelling that replaced the retired `start` form (FIG-2999): the
/// operation answers with the process handle the old expression produced, so
/// a fixture that started a process and awaited the handle keeps its shape.
pub fn start(process: &str, args: Vec<(&str, Expr)>) -> Expr {
    let fields = vec![("definition", process_ref(process)), ("args", record(args))];
    module_call(&["processes"], "start", vec![record(fields)])
}

/// `await processes.signal({ handle, signal, payload })?` — the tool spelling
/// that replaced the retired `signal_run` form (FIG-2999).
pub fn signal_run(run: Expr, name: &str, payload: Expr) -> Expr {
    module_call(
        &["processes"],
        "signal",
        vec![record(vec![
            ("handle", run),
            ("signal", string(name)),
            ("payload", payload),
        ])],
    )
}

/// `await processes.cancel({ handle })?` — the tool spelling that replaced the
/// retired `cancel` form (FIG-2999).
pub fn cancel(handle: Expr) -> Expr {
    module_call(
        &["processes"],
        "cancel",
        vec![record(vec![("handle", handle)])],
    )
}

/// A reference to a declared process by name.
pub fn process_ref(name: &str) -> Expr {
    Expr::ProcessRef {
        process: name.into(),
    }
}

/// A host descriptor constructor, e.g. `timer.Schedule({ .. })`.
pub fn host_descriptor(type_name: &str, input: Expr) -> Expr {
    Expr::HostDescriptorConstructor {
        type_name: type_name.into(),
        input: Box::new(input),
    }
}
