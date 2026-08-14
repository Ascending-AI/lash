use super::*;

pub(super) fn module_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Ident(name) => Some(vec![name.clone()]),
        Expr::Member {
            object,
            property: MemberProperty::Field(field),
        } => {
            let mut path = module_path(object)?;
            path.push(field.clone());
            Some(path)
        }
        _ => None,
    }
}

pub(super) fn static_stdlib_owner(expr: &Expr) -> Option<&str> {
    let Expr::Ident(name) = expr else {
        return None;
    };
    matches!(
        name.as_str(),
        "Object" | "Array" | "String" | "Number" | "JSON" | "Math"
    )
    .then_some(name)
}

pub(super) fn is_known_runtime_global(name: &str) -> bool {
    matches!(
        name,
        "Object" | "Array" | "String" | "Number" | "JSON" | "Math" | "Date" | "Promise"
    )
}

pub(super) fn is_static_stdlib_method(owner: &str, method: &str) -> bool {
    match owner {
        "Object" => matches!(
            method,
            "keys" | "values" | "entries" | "fromEntries" | "hasOwn" | "is"
        ),
        "Array" => matches!(method, "isArray" | "of"),
        "String" => matches!(method, "fromCodePoint"),
        "Number" => matches!(
            method,
            "isFinite" | "isInteger" | "isNaN" | "isSafeInteger" | "parseFloat" | "parseInt"
        ),
        "JSON" => matches!(method, "parse" | "stringify"),
        "Math" => matches!(
            method,
            "abs"
                | "acos"
                | "asin"
                | "cbrt"
                | "ceil"
                | "cos"
                | "exp"
                | "floor"
                | "log"
                | "log10"
                | "log2"
                | "round"
                | "sin"
                | "tan"
                | "trunc"
                | "max"
                | "min"
                | "pow"
                | "sqrt"
                | "sign"
        ),
        _ => false,
    }
}

pub(super) fn is_instance_stdlib_method(method: &str) -> bool {
    matches!(
        method,
        "at" | "charAt"
            | "charCodeAt"
            | "codePointAt"
            | "concat"
            | "endsWith"
            | "includes"
            | "indexOf"
            | "join"
            | "lastIndexOf"
            | "map"
            | "padEnd"
            | "padStart"
            | "repeat"
            | "replace"
            | "replaceAll"
            | "slice"
            | "split"
            | "startsWith"
            | "substring"
            | "toLowerCase"
            | "toUpperCase"
            | "toString"
            | "trim"
            | "trimEnd"
            | "trimStart"
            | "valueOf"
    )
}

pub(super) fn literal_supports_instance_method(expr: &Expr, method: &str) -> bool {
    match expr {
        Expr::String(_) => matches!(
            method,
            "at" | "charAt"
                | "charCodeAt"
                | "codePointAt"
                | "concat"
                | "endsWith"
                | "includes"
                | "indexOf"
                | "lastIndexOf"
                | "padEnd"
                | "padStart"
                | "repeat"
                | "replace"
                | "replaceAll"
                | "slice"
                | "split"
                | "startsWith"
                | "substring"
                | "toLowerCase"
                | "toUpperCase"
                | "trim"
                | "trimStart"
                | "trimEnd"
                | "toString"
                | "valueOf"
        ),
        Expr::Array(_) => matches!(
            method,
            "at" | "concat"
                | "includes"
                | "indexOf"
                | "join"
                | "lastIndexOf"
                | "map"
                | "slice"
                | "toString"
        ),
        Expr::Number(_) | Expr::Bool(_) | Expr::Null | Expr::Undefined | Expr::Object(_) => {
            matches!(method, "toString" | "valueOf")
        }
        _ => true,
    }
}

pub(super) fn has_literal_stdlib_receiver(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::String(_)
            | Expr::Number(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Array(_)
            | Expr::Object(_)
    )
}

pub(super) fn journaled_runtime_call(operation: &str) -> LashExpr {
    LashExpr::ReceiverCall {
        receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::resolved(
            vec!["__typescript_runtime".into()],
            "typescript.Runtime",
            "builtin",
        ))),
        operation: operation.into(),
        args: Vec::new(),
    }
}

pub(super) fn all_settled_results(items: LashExpr) -> LashExpr {
    let result_name = format!("{GENERATED_BINDING_PREFIX}settled");
    let result = || LashExpr::Variable(result_name.as_str().into());
    let field = |name: &str| LashExpr::Field {
        target: Box::new(result()),
        field: name.into(),
    };
    LashExpr::Map {
        items: Box::new(items),
        function: Box::new(LashExpr::Function(Box::new(FunctionExpr {
            name: None,
            params: vec![result_name.as_str().into()],
            captures: Vec::new(),
            body: Box::new(LashExpr::If {
                condition: Box::new(field("ok")),
                then_block: Box::new(LashExpr::Record(vec![
                    ("status".into(), LashExpr::String("fulfilled".into())),
                    ("value".into(), field("value")),
                ])),
                else_block: Box::new(LashExpr::Record(vec![
                    ("status".into(), LashExpr::String("rejected".into())),
                    (
                        "reason".into(),
                        LashExpr::Record(vec![
                            ("name".into(), LashExpr::String("EffectError".into())),
                            ("message".into(), field("error")),
                            // An allSettled leaf is never unwrapped, so it
                            // cannot have failed the way an unwrap does. The
                            // message carries the host's own text, which is the
                            // only identity the effect-host contract exposes
                            // today; a finer code needs a code channel on
                            // ExecutionHostError, which every host would have
                            // to populate.
                            (
                                "code".into(),
                                LashExpr::String("ResourceOperationFailed".into()),
                            ),
                            (
                                "details".into(),
                                LashExpr::Record(vec![
                                    ("kind".into(), LashExpr::String("effect".into())),
                                    (
                                        "operation".into(),
                                        LashExpr::String("resource_batch".into()),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                ])),
            }),
        }))),
    }
}

pub(super) fn settle_aggregate_leaves(expr: LashExpr) -> LashExpr {
    match expr {
        LashExpr::List(items) => LashExpr::List(
            items
                .into_iter()
                .map(|item| match item {
                    LashExpr::ReceiverCall { .. } => item,
                    value => LashExpr::Record(vec![
                        ("ok".into(), LashExpr::Bool(true)),
                        ("value".into(), value),
                    ]),
                })
                .collect(),
        ),
        _ => unreachable!("Promise aggregate iterable is validated as an array"),
    }
}

pub(super) fn has_aggregate_effect_leaf(expr: &LashExpr) -> bool {
    matches!(expr, LashExpr::ReceiverCall { .. }) || expr.children().any(has_aggregate_effect_leaf)
}

pub(super) fn has_nested_aggregate_effect(expr: &LashExpr) -> bool {
    let (LashExpr::List(items) | LashExpr::Tuple(items)) = expr else {
        return true;
    };
    items.iter().any(|item| {
        !matches!(item, LashExpr::ReceiverCall { .. }) && has_aggregate_effect_leaf(item)
    })
}

pub(super) fn has_unsupported_aggregate_effect(expr: &LashExpr) -> bool {
    matches!(
        expr,
        LashExpr::Await(_)
            | LashExpr::SleepFor(_)
            | LashExpr::SleepUntil(_)
            | LashExpr::WaitSignal { .. }
            | LashExpr::SignalRun { .. }
            | LashExpr::Wake(_)
            | LashExpr::StartProcess(_)
            | LashExpr::Cancel(_)
            | LashExpr::Finish(_)
            | LashExpr::Fail(_)
    ) || expr.children().any(has_unsupported_aggregate_effect)
}

pub(super) fn has_unbatchable_aggregate_value(expr: &LashExpr) -> bool {
    matches!(
        expr,
        LashExpr::Function(_)
            | LashExpr::Call { .. }
            | LashExpr::Map { .. }
            | LashExpr::Try(_)
            | LashExpr::Throw(_)
            | LashExpr::Return(_)
            | LashExpr::Block(_)
            | LashExpr::Assign { .. }
            | LashExpr::For { .. }
            | LashExpr::ListComprehension { .. }
            | LashExpr::While { .. }
            | LashExpr::Break
            | LashExpr::Continue
            | LashExpr::Print(_)
            | LashExpr::Yield(_)
    ) || expr.children().any(has_unbatchable_aggregate_value)
}

pub(super) fn unwrap_aggregate_leaves(expr: LashExpr) -> LashExpr {
    match expr {
        LashExpr::ReceiverCall { .. } => LashExpr::ResultUnwrap(Box::new(expr)),
        LashExpr::List(items) => {
            LashExpr::List(items.into_iter().map(unwrap_aggregate_leaves).collect())
        }
        LashExpr::Tuple(items) => {
            LashExpr::Tuple(items.into_iter().map(unwrap_aggregate_leaves).collect())
        }
        LashExpr::Record(entries) => LashExpr::Record(
            entries
                .into_iter()
                .map(|(key, value)| (key, unwrap_aggregate_leaves(value)))
                .collect(),
        ),
        value => value,
    }
}
