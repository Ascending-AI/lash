//! The generated shapes the TypeScript lowerer emits and the authored
//! spellings that re-lower to them: `sugar` tries each re-sugar in turn and
//! reports `Ok(None)` for an expression that is none of them.

use lash_vm::{Expr, FunctionExpr, ResourceRefExpr, StructuralRole};

use crate::signatures::INSTANCE_STDLIB_SIGNATURES;

use super::templates::{template_parts, template_text};
use super::{
    Printed, Printer, TypeScriptSourceError, is_lowered_binding, javascript_binary_op, stdlib_call,
};

impl<'p> Printer<'p> {
    /// Re-sugar one lowered shape, or `Ok(None)` if this is not one.
    pub(super) fn sugar(&self, expression: &Expr) -> Result<Option<String>, TypeScriptSourceError> {
        // `await x` on a value that may be a pending promise.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_await_pending"
            && let [value] = args.as_slice()
        {
            return Ok(Some(format!("await {}", self.unary_operand(value)?)));
        }
        if let Expr::Print(inner) = expression
            && let Some(args) = stdlib_call(inner, "__consoleObservationText")
        {
            return Ok(Some(format!("console.log({})", self.arguments(args)?)));
        }
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_await_array"
            && let [items, Expr::String(method)] = args.as_slice()
        {
            return Ok(Some(format!(
                "await Promise.{method}({})",
                self.expression(items)?
            )));
        }
        // `await Promise.allSettled(items)` on a plain array: the aggregate
        // awaits each pending leaf, and a generated mapper folds each
        // outcome into a `{status, value|reason}` record — together they are
        // exactly what the spelling lowers to.
        if let Expr::Map { items, function } = expression
            && let Some(source) = all_settled_results_source(items, function)
        {
            return Ok(Some(format!(
                "await Promise.allSettled({})",
                self.expression(source)?
            )));
        }
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_pending_timer"
            && let [duration] = args.as_slice()
        {
            return Ok(Some(format!("sleep({})", self.expression(duration)?)));
        }
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_pending_tool"
            && let [call @ Expr::ReceiverCall { .. }] = args.as_slice()
        {
            return Ok(Some(self.expression(call)?));
        }
        // `arguments` where the function binds none: the mention lowers to
        // the argv read inline, and the authored identifier reads back to
        // the same call.
        if stdlib_call(expression, "Lash.Arguments").is_some_and(|args| args.is_empty()) {
            return Ok(Some("arguments".to_string()));
        }
        // A function with defaults or a rest parameter carries its arity in
        // a `__lash_vm_closure` wrap; the signature the function spells
        // reproduces both, so the wrap drops away.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_closure"
            && let [
                Expr::Function(function),
                Expr::Number(arity),
                Expr::Bool(accepts_rest),
            ] = args.as_slice()
        {
            let arity = if arity.fract() == 0.0 && *arity >= 0.0 && *arity <= usize::MAX as f64 {
                *arity as usize
            } else {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a closure arity that is not a count",
                });
            };
            return self
                .function_literal(function, Some((arity, *accepts_rest)))
                .map(Some);
        }
        if let Some(args) = stdlib_call(expression, "Lash.SparseArray") {
            return self.sparse_array(args).map(Some);
        }
        if let Some([key, receiver]) = stdlib_call(expression, "Lash.HasProperty") {
            return Ok(Some(format!(
                "({} in {})",
                self.binary_operand(key)?,
                self.binary_operand(receiver)?
            )));
        }
        if let Some(sugared) = self.property_presence(expression)? {
            return Ok(Some(sugared));
        }
        if let Some(sugared) = self.json_stringify(expression)? {
            return Ok(Some(sugared));
        }
        // `globalThis.name`, read live through the root-global read.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_global_get"
            && let [Expr::String(global)] = args.as_slice()
        {
            return Ok(Some(format!(
                "globalThis.{}",
                self.identifier("global", global.as_str())?
            )));
        }
        // A member reference's key is already converted once, at member
        // evaluation; the authored index spells it.
        if let Some([key]) = stdlib_call(expression, "Lash.ToPropertyKey") {
            return Ok(Some(self.expression(key)?));
        }
        if let Some(sugared) = self.collection_transform(expression)? {
            return Ok(Some(sugared));
        }
        if let Some((quasis, holes)) = template_parts(expression) {
            let mut out = String::from("`");
            for (index, quasi) in quasis.iter().enumerate() {
                out.push_str(&template_text(quasi));
                if let Some(hole) = holes.get(index) {
                    out.push_str(&format!("${{{}}}", self.expression(hole)?));
                }
            }
            out.push('`');
            return Ok(Some(out));
        }
        if let Some((target, operator, value)) = attribute_assignment(expression)? {
            return Ok(Some(format!(
                "({target} {operator} {})",
                self.expression(value)?
            )));
        }
        // An instance standard-library call, `receiver.method(..)`.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__lash_vm_stdlib"
            && let [Expr::String(method), receiver, args @ ..] = args.as_slice()
            && INSTANCE_STDLIB_SIGNATURES
                .iter()
                .any(|signature| signature.method == method.as_str())
        {
            let args = self.arguments(args)?;
            return Ok(Some(format!(
                "{}.{method}({})",
                self.member_target(receiver)?,
                args
            )));
        }
        Ok(None)
    }

    /// A call's arguments, comma-separated.
    pub(super) fn arguments(&self, args: &[Expr]) -> Printed {
        let args = args
            .iter()
            .map(|arg| self.expression(arg))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(args.join(", "))
    }

    pub(super) fn resource_ref(&self, resource: &ResourceRefExpr) -> Printed {
        let path = if resource.path.is_empty() {
            resource
                .alias
                .split('.')
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        } else {
            resource
                .path
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let Some((root, rest)) = path.split_first() else {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "an unnamed resource",
            });
        };
        let mut out = self.identifier("resource", root)?;
        for segment in rest {
            out.push('.');
            out.push_str(&self.identifier("resource", segment)?);
        }
        Ok(out)
    }
}

/// The authored target spelling, assignment operator (`=`, or `op=` for an
/// update) and right-hand side of an attribute-assignment role.
pub(super) fn attribute_assignment(
    expression: &Expr,
) -> Result<Option<(String, String, &Expr)>, TypeScriptSourceError> {
    let Expr::Role {
        role: StructuralRole::AttributeAssign,
        expr,
    } = expression
    else {
        return Ok(None);
    };
    let Some(parts) = lash_vm::AttributeAssignParts::of(expr) else {
        return Ok(None);
    };
    let printer = Printer::plain();
    let object = printer.member_target(parts.object)?;
    let target = match parts.step {
        lash_vm::AttributeStep::Field(field) => {
            format!("{object}.{}", printer.identifier("field", field.as_str())?)
        }
        lash_vm::AttributeStep::Index(index) => {
            format!("{object}[{}]", printer.expression(index)?)
        }
    };
    Ok(Some(match parts.update {
        Some(update) => (
            target,
            format!("{}=", javascript_binary_op(update.operator.coercing_op())),
            update.operand,
        ),
        None => (target, "=".to_string(), parts.value),
    }))
}

/// The array an `allSettled` aggregate's result mapper wraps, when `items`
/// and `function` are the pair `await Promise.allSettled(array)` lowers to:
/// the await over `"allSettled"`, then a one-parameter map folding each
/// leaf's outcome into a `{status, value|reason}` record.
fn all_settled_results_source<'a>(items: &'a Expr, function: &'a Expr) -> Option<&'a Expr> {
    let Expr::BuiltinCall { name, args } = items else {
        return None;
    };
    let [source, Expr::String(mode)] = args.as_slice() else {
        return None;
    };
    if name.as_str() != "__lash_vm_await_array" || mode.as_str() != "allSettled" {
        return None;
    }
    let Expr::Function(function) = function else {
        return None;
    };
    if function.name.is_some()
        || function.receiver.is_some()
        || !function.captures.is_empty()
        || function.params.len() != 1
    {
        return None;
    }
    let result = function.params[0].as_str();
    if !is_lowered_binding(result) {
        return None;
    }
    let field = |expression: &Expr, name: &str| {
        matches!(expression, Expr::Field { target, field }
            if matches!(target.as_ref(), Expr::Variable(found) if found.as_str() == result)
                && field.as_str() == name)
    };
    let record = |expression: &Expr, status: &str, payload: &str| {
        let Expr::Record(fields) = expression else {
            return false;
        };
        fields.len() == 2
            && fields.iter().any(|(name, value)| {
                name.as_str() == "status"
                    && matches!(value, Expr::String(found) if found.as_str() == status)
            })
            && fields
                .iter()
                .any(|(name, value)| name.as_str() == payload && field(value, "value"))
    };
    let Expr::If {
        condition,
        then_block,
        else_block,
    } = function.body.as_ref()
    else {
        return None;
    };
    if !field(condition, "ok") {
        return None;
    }
    // Fulfilled: `{status: "fulfilled", value: result.value}`.
    if !record(then_block, "fulfilled", "value") {
        return None;
    }
    // Rejected: `{status: "rejected", reason: new EffectError(result.error,
    // {cause: result.cause})}`.
    let Expr::Record(fields) = else_block.as_ref() else {
        return None;
    };
    let has_status = fields.iter().any(|(name, value)| {
        name.as_str() == "status"
            && matches!(value, Expr::String(found) if found.as_str() == "rejected")
    });
    let has_reason = fields.iter().any(|(name, value)| {
        name.as_str() == "reason"
            && matches!(value, Expr::BuiltinCall { name, args }
                if name.as_str() == "__lash_vm_heap_new"
                    && matches!(args.as_slice(),
                        [Expr::String(ctor), error, Expr::Record(cause)]
                            if ctor.as_str() == "EffectError"
                                && field(error, "error")
                                && cause.len() == 1
                                && cause[0].0.as_str() == "cause"
                                && field(&cause[0].1, "cause")))
    });
    (fields.len() == 2 && has_status && has_reason).then_some(source)
}

/// A `__lash_vm_closure` wrap prints as the function it carries — the
/// arrow is an AssignmentExpression, so operand positions that parenthesize
/// a bare `Expr::Function` must parenthesize the wrap the same way.
pub(super) fn is_closure_wrap(expression: &Expr) -> bool {
    matches!(expression, Expr::BuiltinCall { name, .. } if name.as_str() == "__lash_vm_closure")
}

/// The function a `__lash_vm_closure` wrap carries together with its
/// recorded `(required arity, accepts rest)`, or the bare `Expr::Function`
/// with none.
pub(super) fn closure_function(
    expression: &Expr,
) -> Option<(&FunctionExpr, Option<(usize, bool)>)> {
    match expression {
        Expr::Function(function) => Some((function, None)),
        Expr::BuiltinCall { name, args }
            if name.as_str() == "__lash_vm_closure"
                && let [
                    Expr::Function(function),
                    Expr::Number(arity),
                    Expr::Bool(rest),
                ] = args.as_slice()
                && arity.fract() == 0.0
                && *arity >= 0.0
                && *arity <= usize::MAX as f64 =>
        {
            Some((function, Some((*arity as usize, *rest))))
        }
        _ => None,
    }
}
