use super::*;
use crate::signatures::LiteralReceivers;

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
        "Object" | "Array" | "String" | "Number" | "JSON" | "Math" | "Map" | "Date" | "URL"
    )
    .then_some(name)
}

pub(super) fn is_known_runtime_global(name: &str) -> bool {
    matches!(
        name,
        "Object"
            | "Array"
            | "String"
            | "Number"
            | "JSON"
            | "Math"
            | "Map"
            | "Date"
            | "Promise"
            | "URL"
            | "URLSearchParams"
            | "RegExp"
    )
}

/// Every ECMA global namespace an author could reasonably call a static method
/// on, whether or not the dialect implements it.
///
/// A tool module is addressed by an explicit path root, and none of these names
/// is one. Without this list a call like `Error.isError(x)` fell through to the
/// tool-call branch and reported `TS_AWAIT_REQUIRED` — an instruction to add
/// `await` to a method that does not exist, which sends the reader to fix the
/// one thing that is not wrong. The census names `TS_METHOD_UNSUPPORTED` for
/// exactly these rows, and the census probes now hold that claim to the code.
pub(super) fn is_ecma_global_namespace(name: &str) -> bool {
    is_known_runtime_global(name)
        || matches!(
            name,
            "Error"
                | "AggregateError"
                | "EvalError"
                | "RangeError"
                | "ReferenceError"
                | "SyntaxError"
                | "TypeError"
                | "URIError"
                | "Set"
                | "WeakMap"
                | "WeakSet"
                | "WeakRef"
                | "FinalizationRegistry"
                | "Symbol"
                | "Reflect"
                | "Proxy"
                | "BigInt"
                | "Boolean"
                | "Function"
                | "Iterator"
                | "AsyncFunction"
                | "Temporal"
                | "Intl"
                | "Atomics"
                | "ArrayBuffer"
                | "SharedArrayBuffer"
                | "DataView"
                | "Int8Array"
                | "Uint8Array"
                | "Uint8ClampedArray"
                | "Int16Array"
                | "Uint16Array"
                | "Int32Array"
                | "Uint32Array"
                | "Float16Array"
                | "Float32Array"
                | "Float64Array"
                | "BigInt64Array"
                | "BigUint64Array"
                | "globalThis"
        )
}

pub(super) fn is_static_stdlib_method(owner: &str, method: &str) -> bool {
    crate::signatures::STATIC_STDLIB_SIGNATURES
        .iter()
        .any(|signature| signature.owner == owner && signature.method == method)
}

/// Every instance standard-library method the lowerer accepts.
///
/// Projected off [`crate::signatures::INSTANCE_STDLIB_SIGNATURES`] so the
/// inventory cannot drift between the signature table, the lowerer's allowlist,
/// and external consumers.
pub(super) fn instance_stdlib_methods() -> &'static [&'static str] {
    static METHODS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
        crate::signatures::INSTANCE_STDLIB_SIGNATURES
            .iter()
            .map(|signature| signature.method)
            .collect()
    });
    &METHODS
}

pub(super) fn is_instance_stdlib_method(method: &str) -> bool {
    crate::signatures::INSTANCE_STDLIB_SIGNATURES
        .iter()
        .any(|signature| signature.method == method)
}

/// The literal receiver shape `expr` is written as, if it is written as one.
///
/// A receiver the lowerer cannot type on sight — a binding, a chained call, a
/// constructor — has no kind here, and the instance surface is open to it.
fn literal_receiver_kind(expr: &Expr) -> Option<LiteralReceivers> {
    match expr {
        Expr::String(_) => Some(LiteralReceivers::STRING),
        Expr::Array(_) => Some(LiteralReceivers::ARRAY),
        Expr::Number(_) => Some(LiteralReceivers::NUMBER),
        Expr::Bool(_) | Expr::Null | Expr::Undefined | Expr::Object(_) => {
            Some(LiteralReceivers::OTHER)
        }
        _ => None,
    }
}

/// Whether a literal receiver carries `method`.
///
/// Projected off the receiver-kind column of
/// [`crate::signatures::INSTANCE_STDLIB_SIGNATURES`], so the per-shape answer
/// and the advertised signature cannot disagree the way they did before
/// FIG-1718.
pub(super) fn literal_supports_instance_method(expr: &Expr, method: &str) -> bool {
    let Some(kind) = literal_receiver_kind(expr) else {
        return true;
    };
    crate::signatures::INSTANCE_STDLIB_SIGNATURES
        .iter()
        .any(|signature| signature.method == method && signature.receivers.contains(kind))
}

pub(super) fn has_literal_stdlib_receiver(expr: &Expr) -> bool {
    literal_receiver_kind(expr).is_some()
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
                    // A rejected leaf's reason is the same idiomatic Error the
                    // awaited form throws: `instanceof Error`, `name` of
                    // `EffectError`, the host's own text as `message`, and the
                    // typed payload on `cause`. The brand is a
                    // compiler-emitted discriminator, which is why the dialect
                    // can mint it while `new EffectError(...)` stays refused.
                    (
                        "reason".into(),
                        LashExpr::BuiltinCall {
                            name: "__typescript_heap_new".into(),
                            args: vec![
                                LashExpr::String("EffectError".into()),
                                field("error"),
                                LashExpr::Record(vec![(
                                    "cause".into(),
                                    LashExpr::Record(vec![
                                        // An allSettled leaf is never
                                        // unwrapped, so it cannot have failed
                                        // the way an unwrap does. The message
                                        // carries the host's own text, which is
                                        // the only identity the effect-host
                                        // contract exposes today; a finer code
                                        // needs a code channel on
                                        // ExecutionHostError, which every host
                                        // would have to populate.
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
                                )]),
                            ],
                        },
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
