//! Effect classification, naming and small helpers the lens reads both when
//! projecting and when rendering a graph back (FIG-3033).

use super::*;

pub(crate) fn effect_name(expression: &Expr, effect: &WorkflowEffectKind) -> String {
    let descriptor_expression = match expression {
        Expr::ResultUnwrap(inner) => inner.as_ref(),
        _ => expression,
    };
    if let Some((_, label)) = lashlang::execution_site_descriptor(descriptor_expression) {
        return label.into_owned();
    }
    match effect {
        WorkflowEffectKind::AwaitJoin => "await",
        WorkflowEffectKind::Print => "print",
        WorkflowEffectKind::Break => "break",
        WorkflowEffectKind::Continue => "continue",
        // The process control effects are tool calls now, and the retired
        // dialect forms no longer reach this projection; the remaining
        // execution-site effects all carry a compiler descriptor.
        WorkflowEffectKind::WaitSignal
        | WorkflowEffectKind::SleepFor
        | WorkflowEffectKind::SleepUntil
        | WorkflowEffectKind::Yield => {
            unreachable!("execution-site effects must have a compiler descriptor")
        }
    }
    .to_string()
}

/// A builtin's display name, never a generated one.
///
/// The lowerer's own builtins carry the reserved generated prefix, which is not
/// a name a user would recognise, so they show as the kind of thing they are.
fn builtin_name(name: &str) -> String {
    match name.strip_prefix(crate::GENERATED_BINDING_PREFIX) {
        Some("await_array") => "await all".to_string(),
        Some(_) => "computation".to_string(),
        None => name.to_string(),
    }
}

pub(crate) fn data_name(expression: &Expr) -> String {
    match expression {
        Expr::BuiltinCall { name, .. } => builtin_name(name),
        Expr::List(_) => "list".to_string(),
        Expr::Record(_) => "record".to_string(),
        Expr::Tuple(_) => "tuple".to_string(),
        Expr::Variable(name) => name.to_string(),
        _ => "data".to_string(),
    }
}

pub(crate) fn computation_name(expression: &Expr) -> String {
    match expression {
        Expr::Tuple(_) => "tuple computation",
        Expr::List(_) => "list computation",
        Expr::Record(_) => "record computation",
        Expr::BuiltinCall { name, .. } => return builtin_name(name),
        Expr::Binary { .. } => "binary computation",
        Expr::Unary { .. } => "unary computation",
        Expr::Field { .. } => "field computation",
        Expr::Index { .. } => "index computation",
        Expr::ResultUnwrap(_) => "result computation",
        _ => "computation",
    }
    .to_string()
}

pub(crate) fn hex_digest(domain: &str, bytes: &[u8]) -> String {
    lash_sansio::core_support::blake3_domain_hash_hex(domain, bytes)
}

/// Purity as the lens means it.
///
/// `await` on a value that may or may not be a promise lowers to a builtin
/// whose arguments are pure, so `is_pure_expr` alone would call an awaited
/// composite a constant and project it as data rather than as the computation
/// it is.
pub(crate) fn is_pure_value(expression: &Expr) -> bool {
    lashlang::is_pure_expr(expression) && !awaits(expression)
}

pub(crate) fn awaits(expression: &Expr) -> bool {
    match expression {
        Expr::Await(_) => true,
        Expr::BuiltinCall { name, .. } if name.as_str() == "__typescript_await_pending" => true,
        _ => expression.children().any(awaits),
    }
}

/// The derived name of an opaque statement node.
pub(crate) fn opaque_name(expression: &Expr) -> &'static str {
    match expression {
        Expr::Try(_) => "try",
        Expr::Throw(_) => "throw",
        Expr::Return(_) => "return",
        Expr::Break => "break",
        Expr::Continue => "continue",
        _ => "statement",
    }
}

/// Rebuild the lowerer's process wrapper around an authored run body.
///
/// The graph shows the authored body; the wrapper that turns an uncaught error
/// into process failure is generated, so it is regenerated here rather than
/// stored.
pub(crate) fn process_wrapper(params: &[lashlang::ProcessParam], body: Expr) -> Expr {
    let error = format!("{}0_process_error", crate::GENERATED_BINDING_PREFIX);
    Expr::Try(Box::new(lashlang::TryExpr {
        body: Box::new(Expr::Finish(Box::new(Expr::Call {
            function: Box::new(Expr::Function(Box::new(lashlang::FunctionExpr {
                name: None,
                params: params.iter().map(|param| param.name.clone()).collect(),
                captures: Vec::new(),
                body: Box::new(body),
            }))),
            args: params
                .iter()
                .map(|param| Expr::Variable(param.name.clone()))
                .collect(),
        }))),
        catch: Some(lashlang::CatchClause {
            binding: error.clone().into(),
            body: Box::new(Expr::Fail(Box::new(Expr::Variable(error.into())))),
        }),
        finally: None,
    }))
}
