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
        "Array" => matches!(method, "isArray" | "from" | "of"),
        "String" => matches!(method, "fromCharCode" | "fromCodePoint"),
        "Number" => matches!(
            method,
            "isFinite" | "isInteger" | "isNaN" | "isSafeInteger" | "parseFloat" | "parseInt"
        ),
        "JSON" => matches!(method, "parse" | "stringify"),
        "Math" => matches!(
            method,
            "abs" | "ceil" | "floor" | "round" | "trunc" | "max" | "min" | "pow" | "sqrt" | "sign"
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
            | "join"
            | "map"
    )
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
                    ("reason".into(), field("error")),
                ])),
            }),
        }))),
    }
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
