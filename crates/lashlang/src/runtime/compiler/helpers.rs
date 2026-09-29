use super::*;

use std::borrow::Cow;

use lash_sansio::ExecutionNodeKind;

pub(crate) const BRANCH_EXECUTION_SITE_KIND: ExecutionNodeKind = ExecutionNodeKind::Branch;
pub(crate) const LOOP_EXECUTION_SITE_KIND: ExecutionNodeKind = ExecutionNodeKind::Loop;
pub const RESOURCE_OPERATION_EXECUTION_SITE_KIND: ExecutionNodeKind =
    ExecutionNodeKind::ResourceOperation;
pub(crate) const STEP_EXECUTION_SITE_KIND: ExecutionNodeKind = ExecutionNodeKind::Step;

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
        "__typescript_global_get" => IntrinsicOp::JavaScriptGlobalGet,
        "__typescript_global_has" => IntrinsicOp::JavaScriptGlobalHas,
        "__typescript_global_set" => IntrinsicOp::JavaScriptGlobalSet,
        "__typescript_cell_new" => IntrinsicOp::BindingCellNew,
        "__typescript_cell_get" => IntrinsicOp::BindingCellGet,
        "__typescript_cell_set" => IntrinsicOp::BindingCellSet,
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

/// `program.spans` keyed the way the compiler looks them up. Declaration
/// bodies are included, so a deferred function body's nodes resolve their own
/// spans without a copy step.
#[cfg(test)]
pub(crate) fn expression_source_spans(program: &Program) -> FxHashMap<AstPath, Span> {
    program
        .spans
        .iter()
        .map(|(path, span)| (path.clone(), *span))
        .collect()
}

pub fn execution_site_descriptor(expr: &Expr) -> Option<(ExecutionNodeKind, Cow<'_, str>)> {
    Some(match expr {
        Expr::ReceiverCall { operation, .. } => (
            RESOURCE_OPERATION_EXECUTION_SITE_KIND,
            Cow::Borrowed(operation.as_str()),
        ),
        Expr::SleepFor(_) => (ExecutionNodeKind::Sleep, Cow::Borrowed("sleep for")),
        Expr::WaitSignal { .. } => (ExecutionNodeKind::Wait, Cow::Borrowed("wait_signal")),
        Expr::Await(handle) if await_wraps_direct_operation(handle) => {
            return None;
        }
        Expr::Await(_) => (ExecutionNodeKind::Wait, Cow::Borrowed("await")),
        Expr::Finish(_) => (ExecutionNodeKind::Terminal, Cow::Borrowed("result")),
        Expr::Fail(_) => (ExecutionNodeKind::Terminal, Cow::Borrowed("failure")),
        Expr::If { .. } => (BRANCH_EXECUTION_SITE_KIND, Cow::Borrowed("if")),
        Expr::For { .. } => (LOOP_EXECUTION_SITE_KIND, Cow::Borrowed("for")),
        Expr::While { .. } => (LOOP_EXECUTION_SITE_KIND, Cow::Borrowed("while")),
        Expr::Call { .. } | Expr::MethodCall { .. } | Expr::ThisCall { .. } => {
            (ExecutionNodeKind::Call, Cow::Borrowed("function call"))
        }
        _ => return None,
    })
}

fn await_wraps_direct_operation(handle: &Expr) -> bool {
    match handle {
        Expr::ReceiverCall { .. } => true,
        Expr::ResultUnwrap(inner) => matches!(inner.as_ref(), Expr::ReceiverCall { .. }),
        _ => false,
    }
}

pub(crate) fn label_attaches_to_concrete_node(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { .. } | Expr::Role { .. } => false,
        Expr::Assign { expr, .. } => label_attaches_to_assignment_value(expr),
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_concrete_node(expr),
        Expr::ReceiverCall { .. }
        | Expr::SleepFor(_)
        | Expr::WaitSignal { .. }
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
        | Expr::List(_)
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
        | Expr::MethodCall { .. }
        | Expr::ThisCall { .. }
        | Expr::FunctionCall { .. }
        | Expr::Map { .. }
        | Expr::Try(_)
        | Expr::Throw(_)
        | Expr::Return(_)
        | Expr::Field { .. }
        | Expr::Index { .. }
        | Expr::JavaScriptUnary { .. }
        | Expr::JavaScriptBinary { .. }
        | Expr::JavaScriptLogical { .. } => false,
    }
}

fn label_attaches_to_assignment_value(expr: &Expr) -> bool {
    match expr {
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_assignment_value(expr),
        Expr::ReceiverCall { .. }
        | Expr::SleepFor(_)
        | Expr::WaitSignal { .. }
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::If { .. } => true,
        _ => false,
    }
}

pub fn is_pure_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } | Expr::Role { expr, .. } => is_pure_expr(expr),
        Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::ProcessRef { .. }
        | Expr::ResourceRef(_) => true,
        // A literal's value is the process it defines, which the linker turns
        // into a reference; its body runs only in that process.
        Expr::ProcessLiteral(_) => true,
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
        | Expr::MethodCall { .. }
        | Expr::ThisCall { .. }
        | Expr::FunctionCall { .. }
        | Expr::Map { .. }
        | Expr::Try(_)
        | Expr::Throw(_)
        | Expr::Return(_) => false,
        Expr::Field { target, .. } => is_pure_expr(target),
        Expr::Index { target, index } => is_pure_expr(target) && is_pure_expr(index),
        Expr::JavaScriptUnary { expr, .. } => is_pure_expr(expr),
        Expr::If {
            condition,
            then_block,
            else_block,
        } => is_pure_expr(condition) && is_pure_expr(then_block) && is_pure_expr(else_block),
        Expr::JavaScriptBinary { left, right, .. }
        | Expr::JavaScriptLogical { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::Block(_)
        | Expr::Assign { .. }
        | Expr::For { .. }
        | Expr::While { .. }
        | Expr::Break
        | Expr::Continue
        | Expr::ReceiverCall { .. }
        | Expr::Await(_)
        | Expr::SleepFor(_)
        | Expr::WaitSignal { .. }
        | Expr::Print(_)
        | Expr::Finish(_)
        | Expr::Fail(_) => false,
    }
}

pub(super) fn is_terminal_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } | Expr::Role { expr, .. } => is_terminal_expr(expr),
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

#[cfg(test)]
mod tests {
    use crate::{TypeExpr, TypeField, runtime::Value, testing::ast_builders as builders};

    #[tokio::test(flavor = "current_thread")]
    async fn type_literal_schema_for_every_producible_type_parses() {
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
            let program = crate::Program::block(vec![crate::Expr::Finish(Box::new(
                builders::type_literal(ty.clone()),
            ))]);
            let compiled = crate::runtime::entry_points::compile_program_internal(&program);
            let mut state = crate::State::new();
            let outcome = crate::runtime::entry_points::execute_compiled_internal(
                &compiled,
                &mut state,
                &crate::testing::harness::EchoHost,
            )
            .await
            .expect("type literal program should run");
            let crate::runtime::ExecutionOutcome::Finished(Value::Record(wrapped)) = outcome else {
                panic!("type literal should finish with the wrapped schema record");
            };
            let schema = wrapped
                .get(crate::LASH_TYPE_KEY)
                .expect("type literal carries the $lash_type schema")
                .clone();
            let schema_json = crate::runtime::to_json_direct(&schema);
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
                TypeExpr::Ref(_) => unreachable!("refs resolve through a binding"),
            };
            assert_eq!(
                schema_json.get("type").and_then(|value| value.as_str()),
                expected_type,
                "schema name for {ty:?}"
            );
            let wrapped_json = serde_json::json!({
                (crate::LASH_TYPE_KEY): schema_json.clone()
            });
            let accepted = lash_sansio::schema_contract::parse_output_schema(Some(&wrapped_json))
                .expect("schema should parse")
                .expect("schema should be present");
            assert_eq!(accepted, schema_json, "schema for {ty:?} was not preserved");
        }
    }
}
