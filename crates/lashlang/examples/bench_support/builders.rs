//! Pure constructors for building the benchmark corpus out of the shared AST.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect, but this corpus
//! measures the IR and the VM, which the ADR keeps. Authoring it in TypeScript
//! would change what is measured rather than how it is spelled, so the
//! scenarios are built straight from the AST, which is the layer they were
//! always about. The dialect is measured beside the corpus instead, in
//! `crates/lashlang/tests/dialect_cost.rs`.
//!
//! Every helper here is a plain constructor. Nothing parses, nothing accepts a
//! string DSL, and nothing supplies a default for a field a scenario measures.

#![allow(dead_code)]

use lashlang::{
    AssignPathStep, AssignTarget, AstString, CatchClause, Declaration, Expr, FunctionDecl,
    FunctionExpr, FunctionParam, JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp,
    LabelMetadata, ProcessDecl, ProcessParam, ProcessSignalDecl, Program, ResourceRefExpr, TryExpr,
    TypeExpr, TypeField,
};

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
        declarations,
        main: Expr::Block(expressions),
        private_bindings: Default::default(),
        spans: Default::default(),
    }
}

/// `process <name>(<params>) { <body> }`, with no signals, return type or label.
pub fn process(name: &str, params: Vec<ProcessParam>, body: Expr) -> Declaration {
    Declaration::Process(ProcessDecl {
        name: name.into(),
        params,
        signals: Vec::new(),
        return_ty: None,
        label: None,
        origin: Default::default(),
        body,
    })
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
        origin: Default::default(),
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
        origin: Default::default(),
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
        origin: Default::default(),
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

pub fn record(fields: Vec<(&str, Expr)>) -> Expr {
    Expr::Record(
        fields
            .into_iter()
            .map(|(name, value)| (AstString::from(name), value))
            .collect(),
    )
}

/// A `{$lash_type: <schema>}` type record as plain IR: `Expr::TypeLiteral` is
/// gone with the surface dialect, but `validate` still reads the schema shape
/// it produced, so the corpus builds the record directly.
pub fn type_literal(ty: TypeExpr) -> Expr {
    Expr::Record(vec![(
        AstString::from(lashlang::LASH_TYPE_KEY),
        type_schema(&ty),
    )])
}

fn type_schema(ty: &TypeExpr) -> Expr {
    let scalar = |name: &str| {
        Expr::Record(vec![(
            AstString::from("type"),
            Expr::String(AstString::from(name)),
        )])
    };
    match ty {
        TypeExpr::Any | TypeExpr::Process(_) | TypeExpr::TriggerHandle(_) => {
            Expr::Record(Vec::new())
        }
        TypeExpr::Str => scalar("string"),
        TypeExpr::Int => scalar("integer"),
        TypeExpr::Float => scalar("number"),
        TypeExpr::Bool => scalar("boolean"),
        TypeExpr::Dict => scalar("object"),
        TypeExpr::Null => scalar("null"),
        TypeExpr::Enum(values) => Expr::Record(vec![
            (
                AstString::from("type"),
                Expr::String(AstString::from("string")),
            ),
            (
                AstString::from("enum"),
                Expr::List(
                    values
                        .iter()
                        .map(|value| Expr::String(value.clone()))
                        .collect(),
                ),
            ),
        ]),
        TypeExpr::List(inner) => Expr::Record(vec![
            (
                AstString::from("type"),
                Expr::String(AstString::from("array")),
            ),
            (AstString::from("items"), type_schema(inner)),
        ]),
        TypeExpr::Object(fields) => Expr::Record(vec![
            (
                AstString::from("type"),
                Expr::String(AstString::from("object")),
            ),
            (
                AstString::from("properties"),
                Expr::Record(
                    fields
                        .iter()
                        .map(|field| (field.name.clone(), type_schema(&field.ty)))
                        .collect(),
                ),
            ),
            (
                AstString::from("required"),
                Expr::List(
                    fields
                        .iter()
                        .filter(|field| !field.optional)
                        .map(|field| Expr::String(field.name.clone()))
                        .collect(),
                ),
            ),
            (AstString::from("additionalProperties"), Expr::Bool(false)),
        ]),
        TypeExpr::Union(variants) => Expr::Record(vec![(
            AstString::from("anyOf"),
            Expr::List(variants.iter().map(type_schema).collect()),
        )]),
        TypeExpr::Ref(name) => Expr::Index {
            target: Box::new(Expr::Variable(name.clone())),
            index: Box::new(Expr::String(AstString::from(lashlang::LASH_TYPE_KEY))),
        },
    }
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

pub fn binary(left: Expr, op: JavaScriptBinaryOp, right: Expr) -> Expr {
    Expr::JavaScriptBinary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

pub fn unary(op: JavaScriptUnaryOp, expr: Expr) -> Expr {
    Expr::JavaScriptUnary {
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

pub fn while_loop(condition: Expr, body: Expr) -> Expr {
    Expr::While {
        condition: Box::new(condition),
        body: Box::new(body),
    }
}

/// `items.map(function)` — what a single-clause comprehension compiled to.
pub fn map(items: Expr, function: Expr) -> Expr {
    Expr::Map {
        items: Box::new(items),
        function: Box::new(function),
    }
}

/// `[...list, ...other]` — list concat under the TypeScript stdlib.
pub fn concat(list: Expr, other: Expr) -> Expr {
    Expr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args: vec![Expr::String("concat".into()), list, other],
    }
}

pub fn logical(left: Expr, op: JavaScriptLogicalOp, right: Expr) -> Expr {
    Expr::JavaScriptLogical {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
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

pub fn await_expr(expr: Expr) -> Expr {
    Expr::Await(Box::new(expr))
}

pub fn unwrap(expr: Expr) -> Expr {
    Expr::ResultUnwrap(Box::new(expr))
}

/// `await processes.cancel({ handle })` — the tool spelling that replaced the
/// retired `cancel` form.
pub fn cancel(expr: Expr) -> Expr {
    await_expr(unwrap(receiver_call(
        var("processes"),
        "cancel",
        vec![record(vec![("handle", expr)])],
    )))
}

pub fn sleep_for(expr: Expr) -> Expr {
    Expr::SleepFor(Box::new(expr))
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

/// A reference to a declared process by name.
pub fn process_ref(name: &str) -> Expr {
    Expr::ProcessRef {
        process: name.into(),
    }
}

/// `processes.start({ definition, args: { tool, ..args } })` — the tool
/// spelling that replaced the retired `start` form. The process reference keeps
/// the declaration live in the linked module; `tool` names the stub the bench
/// host answers with.
pub fn start(process: &str, args: Vec<(&str, Expr)>) -> Expr {
    let mut inner = vec![("tool", string(process))];
    inner.extend(args);
    let fields = vec![
        ("definition", process_ref(process)),
        ("args", record(inner)),
    ];
    await_expr(unwrap(receiver_call(
        var("processes"),
        "start",
        vec![record(fields)],
    )))
}

/// A host descriptor constructor, e.g. `timer.Schedule({ .. })`.
pub fn host_descriptor(type_name: &str, input: Expr) -> Expr {
    Expr::HostDescriptorConstructor {
        type_name: type_name.into(),
        input: Box::new(input),
    }
}
