use super::*;

use std::borrow::Cow;

use crate::workflow_graph::child_path;

pub(crate) const BRANCH_EXECUTION_SITE_KIND: &str = "branch";
pub(crate) const LOOP_EXECUTION_SITE_KIND: &str = "loop";
pub const RESOURCE_OPERATION_EXECUTION_SITE_KIND: &str = "resource_operation";
pub(crate) const STEP_EXECUTION_SITE_KIND: &str = "step";

pub(super) fn expr_supports_forced_effect_site(expr: &Expr) -> bool {
    matches!(expr, Expr::ReceiverCall { .. } | Expr::Await(_))
        || matches!(
            expr,
            Expr::ResultUnwrap(inner)
                if matches!(inner.as_ref(), Expr::ReceiverCall { .. } | Expr::Await(_))
        )
}

/// builtins (the caller decides whether that is an `Unknown` op or a const-fold
/// This is the single name -> op authority shared by `resolve_intrinsic` and the const folder.
pub(super) fn intrinsic_for_builtin(name: &str, argc: usize) -> Option<IntrinsicOp> {
    Some(match name {
        "len" => IntrinsicOp::Len,
        "empty" => IntrinsicOp::Empty,
        "keys" => IntrinsicOp::Keys,
        "values" => IntrinsicOp::Values,
        "contains" => IntrinsicOp::Contains,
        "find" => IntrinsicOp::Find(argc),
        "grep_text" => IntrinsicOp::GrepText,
        "starts_with" => IntrinsicOp::StartsWith,
        "ends_with" => IntrinsicOp::EndsWith,
        "split" => IntrinsicOp::Split,
        "join" => IntrinsicOp::Join,
        "__typescript_split" => IntrinsicOp::JavaScriptSplit,
        "__typescript_join" => IntrinsicOp::JavaScriptJoin,
        "__typescript_stdlib" => IntrinsicOp::JavaScriptStdlib(argc),
        "__typescript_heap_new" => IntrinsicOp::JavaScriptHeapNew(argc),
        "__typescript_heap_instanceof" => IntrinsicOp::JavaScriptHeapInstanceOf,
        "__typescript_heap_delete_member" => IntrinsicOp::JavaScriptHeapDeleteMember,
        "__typescript_regexp" => IntrinsicOp::JavaScriptRegExp(argc),
        "__typescript_global_delete" => IntrinsicOp::JavaScriptGlobalDelete,
        "__typescript_global_has" => IntrinsicOp::JavaScriptGlobalHas,
        "__typescript_global_set" => IntrinsicOp::JavaScriptGlobalSet,
        "__typescript_encode_uri_component" => {
            IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::EncodeComponent)
        }
        "__typescript_decode_uri_component" => {
            IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::DecodeComponent)
        }
        "__typescript_encode_uri" => IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::EncodeUri),
        "__typescript_decode_uri" => IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::DecodeUri),
        "trim" => IntrinsicOp::Trim,
        "slice" => IntrinsicOp::Slice,
        "to_string" => IntrinsicOp::ToString,
        "to_int" => IntrinsicOp::ToInt,
        "to_float" => IntrinsicOp::ToFloat,
        "json_parse" => IntrinsicOp::JsonParse,
        "format" => IntrinsicOp::Format(argc),
        "validate" => IntrinsicOp::Validate,
        "range" => IntrinsicOp::Range(argc),
        "ceil_div" => IntrinsicOp::CeilDiv,
        "floor_div" => IntrinsicOp::FloorDiv,
        "push" => IntrinsicOp::Push,
        "sort" => IntrinsicOp::Sort,
        "sort_by" => IntrinsicOp::SortBy,
        "sum" => IntrinsicOp::Sum,
        "min" => IntrinsicOp::Min,
        "max" => IntrinsicOp::Max,
        "replace" => IntrinsicOp::Replace,
        "lower" => IntrinsicOp::Lower,
        "upper" => IntrinsicOp::Upper,
        "unique" => IntrinsicOp::Unique,
        "reverse" => IntrinsicOp::Reverse,
        _ => return None,
    })
}

/// Recovers the authored parts of a lowered TypeScript `for..of` loop.
///
/// The lowerer compiles `for (const item of source)` as a generated binding
/// over `Lash.ArrayFromIterable(source)`, followed by an assignment from that
/// binding to `item`. The projector and compiler path table use this one
/// recognizer so they assign the same structural paths to authored body nodes.
#[doc(hidden)]
pub fn lowered_for_of_parts<'a>(
    binding: &str,
    iterable: &'a Expr,
    body: &'a Expr,
) -> Option<(&'a str, &'a Expr, &'a [Expr])> {
    if !binding.starts_with("__typescript_") {
        return None;
    }
    let Expr::BuiltinCall { name, args } = iterable else {
        return None;
    };
    let [Expr::String(selector), source] = args.as_slice() else {
        return None;
    };
    if name.as_str() != "__typescript_stdlib" || selector.as_str() != "Lash.ArrayFromIterable" {
        return None;
    }
    let Expr::Block(statements) = body else {
        return None;
    };
    let [Expr::Assign { target, expr }, rest @ ..] = statements.as_slice() else {
        return None;
    };
    if !target.is_simple()
        || !matches!(expr.as_ref(), Expr::Variable(name) if name.as_str() == binding)
    {
        return None;
    }
    Some((target.root.as_str(), source, rest))
}

/// `main`-rooted execution paths, keyed by [`AstPath`]. `LabelAnnotated` is
/// transparent to the lashlang path vocabulary — the annotation is the step —
/// so its inner node maps to the same `LashlangAstPath` its parent does.
pub(super) fn workflow_node_paths(program: &Program) -> FxHashMap<AstPath, LashlangAstPath> {
    let mut paths = FxHashMap::default();
    collect_workflow_block_paths(&program.main, &AstPath::main(Vec::new()), &[], &mut paths);
    paths
}

/// Process programs execute the declaration's full error boundary while the
/// workflow graph addresses the authored function body inside it. Preserve
/// the wrapper prefix in both the compiler AST key and the node path so the
/// runtime and projector mint the same structural id.
pub(super) fn workflow_node_paths_for_process(
    program: &Program,
) -> FxHashMap<AstPath, LashlangAstPath> {
    let Some((prefix, body)) = process_execution_body_path(&program.main) else {
        return workflow_node_paths(program);
    };
    let mut paths = FxHashMap::default();
    collect_workflow_block_paths(body, &AstPath::main(prefix.clone()), &prefix, &mut paths);
    paths
}

fn process_execution_body_path(wrapper: &Expr) -> Option<(Vec<u32>, &Expr)> {
    let Expr::Try(try_expr) = wrapper else {
        return None;
    };
    let crate::TryExpr {
        body,
        catch: Some(crate::CatchClause {
            binding,
            body: catch,
        }),
        finally: None,
    } = try_expr.as_ref()
    else {
        return None;
    };
    let Expr::Fail(caught) = catch.as_ref() else {
        return None;
    };
    if !matches!(caught.as_ref(), Expr::Variable(name) if name == binding) {
        return None;
    }

    let mut path = Vec::new();
    let finish = body.as_ref();
    push_child_index(wrapper, finish, &mut path)?;
    let Expr::Finish(call) = finish else {
        return None;
    };
    let call = call.as_ref();
    push_child_index(finish, call, &mut path)?;
    let Expr::Call { function, .. } = call else {
        return None;
    };
    let function = function.as_ref();
    push_child_index(call, function, &mut path)?;
    let Expr::Function(run) = function else {
        return None;
    };
    push_child_index(function, &run.body, &mut path)?;
    Some((path, &run.body))
}

fn push_child_index(parent: &Expr, child: &Expr, path: &mut Vec<u32>) -> Option<()> {
    let index = parent.children().position(|candidate| {
        std::ptr::eq(std::ptr::from_ref(candidate), std::ptr::from_ref(child))
    })?;
    path.push(u32::try_from(index).ok()?);
    Some(())
}

fn collect_workflow_block_paths(
    expression: &Expr,
    ast_path: &AstPath,
    base_path: &[u32],
    paths: &mut FxHashMap<AstPath, LashlangAstPath>,
) {
    let mut expression = expression;
    let mut ast_path = ast_path.clone();
    let mut base_path = base_path.to_vec();
    let mut unwrapped = false;
    while let Some(inner) = workflow_block_wrapper_inner(expression) {
        expression = inner;
        ast_path = ast_path.child(0);
        base_path.push(0);
        unwrapped = true;
    }
    let mut expressions = match expression {
        Expr::Block(expressions) => expressions.as_slice(),
        expression => std::slice::from_ref(expression),
    };
    if let [rest @ .., last] = expressions
        && (matches!(last, Expr::Undefined) || (unwrapped && is_pure_expr(last)))
    {
        expressions = rest;
    }
    let start = matches!(expression, Expr::Block(_)).then_some(0);
    collect_workflow_statement_paths(expressions, &ast_path, &base_path, start, paths);
}

fn collect_workflow_statement_paths(
    expressions: &[Expr],
    ast_base: &AstPath,
    node_base: &[u32],
    start: Option<u32>,
    paths: &mut FxHashMap<AstPath, LashlangAstPath>,
) {
    for (index, expression) in expressions.iter().enumerate() {
        let mut statement_ast_path = ast_base.clone();
        let mut node_path = node_base.to_vec();
        if let Some(start) = start {
            let step = start + index as u32;
            statement_ast_path = statement_ast_path.child(step);
            node_path.push(step);
        }
        let mut statement = expression;
        while let Some(inner) = authored_workflow_statement(statement) {
            statement = inner;
            statement_ast_path = statement_ast_path.child(0);
            node_path.push(0);
        }
        collect_workflow_node_paths(statement, &statement_ast_path, &node_path, paths);
    }
}

fn collect_workflow_node_paths(
    expression: &Expr,
    ast_path: &AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, LashlangAstPath>,
) {
    map_workflow_node_subtree(expression, ast_path, node_path, paths);

    let (expression, expression_ast_path) = match expression {
        Expr::LabelAnnotated { expr, .. } => (expr.as_ref(), ast_path.child(0)),
        expression => (expression, ast_path.clone()),
    };
    let (value, value_ast_path, value_path) = match expression {
        Expr::Assign { target, expr } => {
            let value_index = target
                .steps
                .iter()
                .filter(|step| matches!(step, AssignPathStep::Index(_)))
                .count() as u32;
            (
                expr.as_ref(),
                expression_ast_path.child(value_index),
                child_path(node_path, value_index),
            )
        }
        expression => (expression, expression_ast_path, node_path.to_vec()),
    };

    match value {
        Expr::If {
            then_block,
            else_block,
            ..
        } => {
            collect_workflow_block_paths(
                then_block,
                &value_ast_path.child(1),
                &child_path(&value_path, 1),
                paths,
            );
            collect_workflow_block_paths(
                else_block,
                &value_ast_path.child(2),
                &child_path(&value_path, 2),
                paths,
            );
        }
        Expr::For {
            binding,
            iterable,
            body,
        } => {
            let body_ast_base = value_ast_path.child(1);
            let body_node_base = child_path(&value_path, 1);
            match lowered_for_of_parts(binding, iterable, body) {
                Some((_, _, [single])) if matches!(single, Expr::Block(_)) => {
                    collect_workflow_block_paths(
                        single,
                        &body_ast_base.child(1),
                        &child_path(&body_node_base, 1),
                        paths,
                    );
                }
                Some((_, _, rest)) => collect_workflow_statement_paths(
                    rest,
                    &body_ast_base,
                    &body_node_base,
                    Some(1),
                    paths,
                ),
                None => collect_workflow_block_paths(body, &body_ast_base, &body_node_base, paths),
            }
        }
        Expr::While { body, .. } => {
            collect_workflow_block_paths(
                body,
                &value_ast_path.child(1),
                &child_path(&value_path, 1),
                paths,
            );
        }
        Expr::ListComprehension { element, clauses } => {
            let index = clauses.len() as u32;
            collect_workflow_block_paths(
                element,
                &value_ast_path.child(index),
                &child_path(&value_path, index),
                paths,
            );
        }
        _ => {}
    }
}

fn map_workflow_node_subtree(
    expression: &Expr,
    ast_path: &AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, LashlangAstPath>,
) {
    paths.insert(ast_path.clone(), LashlangAstPath::from_indices(node_path));
    for (index, child) in expression.children().enumerate() {
        map_workflow_node_subtree(child, &ast_path.child(index as u32), node_path, paths);
    }
}

fn workflow_block_wrapper_inner(expression: &Expr) -> Option<&Expr> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    let inner = match statements.as_slice() {
        [inner @ Expr::Block(_), Expr::Undefined] | [inner @ Expr::Block(_)] => inner,
        _ => return None,
    };
    (!is_lowered_member_assignment(inner)).then_some(inner)
}

fn authored_workflow_statement(expression: &Expr) -> Option<&Expr> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    match statements.as_slice() {
        [single, last] if is_pure_expr(last) && !is_lowered_member_assignment(expression) => {
            Some(single)
        }
        _ => None,
    }
}

fn is_lowered_member_assignment(expression: &Expr) -> bool {
    let Expr::Block(statements) = expression else {
        return false;
    };
    let [
        Expr::Assign {
            target: base_target,
            expr: base,
        },
        Expr::Assign {
            target: result_target,
            ..
        },
        Expr::Assign {
            target: store,
            expr: stored,
        },
        Expr::Variable(completion),
    ] = statements.as_slice()
    else {
        return false;
    };
    base_target.root.starts_with("__typescript_")
        && result_target.root.starts_with("__typescript_")
        && store.root == base_target.root
        && !store.steps.is_empty()
        && matches!(stored.as_ref(), Expr::Variable(name) if *name == result_target.root)
        && matches!(base.as_ref(), Expr::Variable(_))
        && *completion == result_target.root
}

/// `program.spans` keyed the way the compiler looks them up. Declaration
/// bodies are included, so a deferred function body's nodes resolve their own
/// spans without a copy step.
pub(crate) fn expression_source_spans(program: &Program) -> FxHashMap<AstPath, Span> {
    program
        .spans
        .iter()
        .map(|(path, span)| (path.clone(), *span))
        .collect()
}

pub fn execution_site_descriptor(expr: &Expr) -> Option<(&'static str, Cow<'_, str>)> {
    Some(match expr {
        Expr::ReceiverCall { operation, .. } => (
            RESOURCE_OPERATION_EXECUTION_SITE_KIND,
            Cow::Borrowed(operation.as_str()),
        ),
        Expr::SleepFor(_) => ("sleep", Cow::Borrowed("sleep for")),
        Expr::SleepUntil(_) => ("sleep", Cow::Borrowed("sleep until")),
        Expr::WaitSignal { .. } => ("wait", Cow::Borrowed("wait_signal")),
        Expr::Finish(_) => ("terminal", Cow::Borrowed("result")),
        Expr::Fail(_) => ("terminal", Cow::Borrowed("failure")),
        Expr::Yield(_) => ("process_event", Cow::Borrowed("yield")),
        Expr::If { .. } => (BRANCH_EXECUTION_SITE_KIND, Cow::Borrowed("if")),
        Expr::For { .. } => (LOOP_EXECUTION_SITE_KIND, Cow::Borrowed("for")),
        Expr::While { .. } => (LOOP_EXECUTION_SITE_KIND, Cow::Borrowed("while")),
        Expr::Call { .. } => ("call", Cow::Borrowed("function call")),
        _ => return None,
    })
}

pub(crate) fn label_attaches_to_concrete_node(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { .. } => false,
        Expr::Assign { expr, .. } => label_attaches_to_assignment_value(expr),
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_concrete_node(expr),
        Expr::ReceiverCall { .. }
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::Yield(_)
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::If { .. }
        | Expr::For { .. }
        | Expr::While { .. } => true,
        // A literal lowers away before compilation, so it never carries a
        // label; the hoisted declaration it becomes does.
        Expr::ProcessLiteral(_) => false,
        Expr::Block(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::Tuple(_)
        | Expr::List(_)
        | Expr::ListComprehension { .. }
        | Expr::Record(_)
        | Expr::Break
        | Expr::Continue
        | Expr::ProcessRef { .. }
        | Expr::HostDescriptorConstructor { .. }
        | Expr::ResourceRef(_)
        | Expr::Print(_)
        | Expr::BuiltinCall { .. }
        | Expr::Function(_)
        | Expr::Call { .. }
        | Expr::FunctionCall { .. }
        | Expr::Map { .. }
        | Expr::Try(_)
        | Expr::Throw(_)
        | Expr::Return(_)
        | Expr::Field { .. }
        | Expr::Index { .. }
        | Expr::Unary { .. }
        | Expr::Binary { .. }
        | Expr::JavaScriptUnary { .. }
        | Expr::JavaScriptBinary { .. }
        | Expr::JavaScriptLogical { .. }
        | Expr::TypeLiteral(_) => false,
    }
}

fn label_attaches_to_assignment_value(expr: &Expr) -> bool {
    match expr {
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_assignment_value(expr),
        Expr::ReceiverCall { .. }
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::Yield(_)
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::If { .. } => true,
        _ => false,
    }
}

pub fn is_pure_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } => is_pure_expr(expr),
        Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::ProcessRef { .. }
        | Expr::ResourceRef(_) => true,
        Expr::ProcessLiteral(literal) => is_pure_expr(&literal.body),
        Expr::Tuple(items) => items.iter().all(is_pure_expr),
        Expr::List(items) => items.iter().all(is_pure_expr),
        Expr::Record(entries) => entries.iter().all(|(_, value)| is_pure_expr(value)),
        Expr::ResultUnwrap(expr) => is_pure_expr(expr),
        Expr::HostDescriptorConstructor { input, .. } => is_pure_expr(input),
        Expr::BuiltinCall { args, .. } => args.iter().all(is_pure_expr),
        Expr::Function(function) => function.captures.is_empty(),
        // A declared function is effect-free but not free of work: it builds a
        // call frame and may allocate, so it is treated like any other call
        // wherever purity means "safe to skip, duplicate, or reorder".
        Expr::Call { .. }
        | Expr::FunctionCall { .. }
        | Expr::Map { .. }
        | Expr::Try(_)
        | Expr::Throw(_)
        | Expr::Return(_) => false,
        Expr::Field { target, .. } => is_pure_expr(target),
        Expr::Index { target, index } => is_pure_expr(target) && is_pure_expr(index),
        Expr::Unary { expr, .. } => is_pure_expr(expr),
        Expr::JavaScriptUnary { expr, .. } => is_pure_expr(expr),
        Expr::If {
            condition,
            then_block,
            else_block,
        } => is_pure_expr(condition) && is_pure_expr(then_block) && is_pure_expr(else_block),
        Expr::Binary { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::JavaScriptBinary { left, right, .. }
        | Expr::JavaScriptLogical { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::TypeLiteral(ty) => fold_type(ty).is_some(),
        Expr::Block(_)
        | Expr::Assign { .. }
        | Expr::For { .. }
        | Expr::ListComprehension { .. }
        | Expr::While { .. }
        | Expr::Break
        | Expr::Continue
        | Expr::ReceiverCall { .. }
        | Expr::Await(_)
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::Print(_)
        | Expr::Yield(_)
        | Expr::Finish(_)
        | Expr::Fail(_) => false,
    }
}

pub(super) fn contains_type_literal(expr: &Expr) -> bool {
    // `TypeLiteral` is the only node that introduces a type literal directly;
    // every other node contains one only via a child expression. `children()`
    // already yields an `Assign` target's dynamic index steps, so the generic
    // structural recursion covers the path-assignment case too.
    matches!(expr, Expr::TypeLiteral(_)) || expr.children().any(contains_type_literal)
}

/// The JSON-Schema keys used by the language's type-schema builders. Scalar
/// type names live in [`SchemaScalarKind`]; these keys are shared by the
/// compile-time builder ([`fold_type`]) and runtime instruction builder
/// ([`Compiler::compile_type_expr`]).
pub(super) mod schema_keys {
    pub(crate) const TYPE: &str = "type";
    pub(crate) const ITEMS: &str = "items";
    pub(crate) const PROPERTIES: &str = "properties";
    pub(crate) const REQUIRED: &str = "required";
    pub(crate) const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
    pub(crate) const ANY_OF: &str = "anyOf";
    pub(crate) const ENUM: &str = "enum";
}

/// Best-effort compile-time construction of a JSON-Schema Value for a
/// [`TypeExpr`]. This is the single authority for the language's type -> schema
/// shape; the runtime instruction builder mirrors only the dynamic `Ref` paths
/// and shares the same key vocabulary ([`schema_keys`]).
///
/// Returns `None` when the expression contains a [`TypeExpr::Ref`] (or a nested
/// composite that contains one) — those must be resolved at runtime via
/// [`Instruction::ResolveTypeRef`].
pub(super) fn fold_type(ty: &TypeExpr) -> Option<Value> {
    use schema_keys::*;
    match ty {
        TypeExpr::Any => Some(interned_scalar_schema(None)),
        TypeExpr::Str => Some(interned_scalar_schema(Some(SchemaScalarKind::String))),
        TypeExpr::Int => Some(interned_scalar_schema(Some(SchemaScalarKind::Integer))),
        TypeExpr::Float => Some(interned_scalar_schema(Some(SchemaScalarKind::Number))),
        TypeExpr::Bool => Some(interned_scalar_schema(Some(SchemaScalarKind::Boolean))),
        TypeExpr::Dict => Some(interned_scalar_schema(Some(SchemaScalarKind::Object))),
        TypeExpr::Null => Some(interned_scalar_schema(Some(SchemaScalarKind::Null))),
        TypeExpr::Enum(values) => {
            let mut rec = record_with_capacity(2);
            rec.insert(
                TYPE.into(),
                Value::String(SchemaScalarKind::String.as_schema_name().into()),
            );
            let items: Vec<Value> = values
                .iter()
                .map(|v| Value::String(v.clone().into()))
                .collect();
            rec.insert(ENUM.into(), Value::List(items.into()));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::List(inner) => {
            let inner_value = fold_type(inner)?;
            let mut rec = record_with_capacity(2);
            rec.insert(
                TYPE.into(),
                Value::String(SchemaScalarKind::Array.as_schema_name().into()),
            );
            rec.insert(ITEMS.into(), inner_value);
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Object(fields) => {
            let mut properties = record_with_capacity(fields.len());
            for field in fields {
                properties.insert(field.name.to_string(), fold_type(&field.ty)?);
            }
            let required: Vec<Value> = fields
                .iter()
                .filter(|f| !f.optional)
                .map(|f| Value::String(f.name.clone().into()))
                .collect();
            let mut rec = record_with_capacity(4);
            rec.insert(
                TYPE.into(),
                Value::String(SchemaScalarKind::Object.as_schema_name().into()),
            );
            rec.insert(PROPERTIES.into(), Value::Record(Arc::new(properties)));
            rec.insert(REQUIRED.into(), Value::List(required.into()));
            rec.insert(ADDITIONAL_PROPERTIES.into(), Value::Bool(false));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Union(variants) => {
            let folded: Option<Vec<Value>> = variants.iter().map(fold_type).collect();
            let folded = folded?;
            let mut rec = record_with_capacity(1);
            rec.insert(ANY_OF.into(), Value::List(folded.into()));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Process(_) | TypeExpr::TriggerHandle(_) => Some(interned_scalar_schema(None)),
        TypeExpr::Ref(_) => None,
    }
}

pub(super) fn wrap_type_schema_value(schema: Value) -> Value {
    let mut wrapper = record_with_capacity(1);
    wrapper.insert(LASH_TYPE_KEY.to_string(), schema);
    Value::Record(Arc::new(wrapper))
}

pub(super) fn is_terminal_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } => is_terminal_expr(expr),
        Expr::Finish(_) | Expr::Fail(_) => true,
        Expr::Block(expressions) => expressions.last().is_some_and(is_terminal_expr),
        Expr::If {
            then_block,
            else_block,
            ..
        } => is_terminal_expr(then_block) && is_terminal_expr(else_block),
        _ => false,
    }
}

/// All sites referencing `str` point at the same `Arc<Record>`, so emitting a Type literal
/// with N string fields allocates one record, not N.
pub(super) fn interned_scalar_schema(kind: Option<SchemaScalarKind>) -> Value {
    static CACHE: OnceLock<[Value; 8]> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let build = |kind: SchemaScalarKind| {
            let mut rec = record_with_capacity(1);
            rec.insert(
                schema_keys::TYPE.into(),
                Value::String(kind.as_schema_name().into()),
            );
            Value::Record(Arc::new(rec))
        };
        [
            Value::Record(Arc::new(record_with_capacity(0))),
            build(SchemaScalarKind::String),
            build(SchemaScalarKind::Number),
            build(SchemaScalarKind::Integer),
            build(SchemaScalarKind::Boolean),
            build(SchemaScalarKind::Array),
            build(SchemaScalarKind::Object),
            build(SchemaScalarKind::Null),
        ]
    });
    let index = match kind {
        None => 0,
        Some(SchemaScalarKind::String) => 1,
        Some(SchemaScalarKind::Number) => 2,
        Some(SchemaScalarKind::Integer) => 3,
        Some(SchemaScalarKind::Boolean) => 4,
        Some(SchemaScalarKind::Array) => 5,
        Some(SchemaScalarKind::Object) => 6,
        Some(SchemaScalarKind::Null) => 7,
    };
    cache[index].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TypeExpr, TypeField};

    #[test]
    fn typed_output_accepts_every_producible_type_schema() {
        let types = vec![
            TypeExpr::Any,
            TypeExpr::Str,
            TypeExpr::Int,
            TypeExpr::Float,
            TypeExpr::Bool,
            TypeExpr::Dict,
            TypeExpr::Null,
            TypeExpr::Enum(vec!["ready".into(), "done".into()]),
            TypeExpr::List(Box::new(TypeExpr::Str)),
            TypeExpr::Object(vec![TypeField {
                name: "value".into(),
                ty: TypeExpr::Int,
                optional: false,
            }]),
            TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Null]),
            TypeExpr::Process(crate::ProcessType::known(
                crate::ProcessSignature::try_new(Vec::new(), TypeExpr::Str).unwrap(),
            )),
            TypeExpr::TriggerHandle(Box::new(TypeExpr::Str)),
        ];

        for ty in types {
            let schema = fold_type(&ty).expect("all listed types are foldable");
            let schema_json = crate::runtime::to_json_direct(&schema);
            let wrapped = serde_json::json!({
                (crate::LASH_TYPE_KEY): schema_json.clone()
            });
            let expected_type = match &ty {
                TypeExpr::Any
                | TypeExpr::Union(_)
                | TypeExpr::Process(_)
                | TypeExpr::TriggerHandle(_) => None,
                TypeExpr::Str | TypeExpr::Enum(_) => Some("string"),
                TypeExpr::Int => Some("integer"),
                TypeExpr::Float => Some("number"),
                TypeExpr::Bool => Some("boolean"),
                TypeExpr::Dict | TypeExpr::Object(_) => Some("object"),
                TypeExpr::Null => Some("null"),
                TypeExpr::List(_) => Some("array"),
                TypeExpr::Ref(_) => unreachable!("refs are not foldable"),
            };
            assert_eq!(
                wrapped[crate::LASH_TYPE_KEY]
                    .get("type")
                    .and_then(|value| value.as_str()),
                expected_type,
                "producer schema name for {ty:?}"
            );
            let accepted = crate::parse_output_schema(Some(&wrapped))
                .expect("producer schema should parse")
                .expect("producer schema should be present");
            assert_eq!(accepted, schema_json, "schema for {ty:?} was not preserved");
        }
    }
}
