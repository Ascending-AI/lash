//! Spread arguments to builtin functions and methods (FIG-3627).
//!
//! A call with a spread argument has an argument count known only at run
//! time. A call to a function the program defines already takes its
//! arguments as a list (`__typescript_call_dynamic`). A builtin is dispatched
//! by the standard library, `__typescript_stdlib(method, receiver?, ...args)`,
//! so a spread call lowers the builtin call once, with one marker per
//! argument, to learn that dispatch, and then replaces the markers with the
//! runtime argument list: `__typescript_stdlib("Lash.Apply", method,
//! receiver?, arguments)`. The VM spreads `arguments` into the same dispatch,
//! so `Math.max(...xs)` runs exactly as `Math.max(x0, x1, ...)` would.
//!
//! A builtin whose call does not lower to that dispatch (a callback method, a
//! coercion, an agent primitive) is refused by name, because its lowering
//! depends on how many arguments it has.

use super::*;

impl Lowerer {
    /// The call `callee(...args)` with a spread argument, when `callee` names
    /// a builtin: `Ok(None)` leaves a call to a program-defined function to
    /// the dynamic path.
    pub(super) fn lower_builtin_spread_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
    ) -> Result<Option<LashExpr>, Diagnostic> {
        let name = match callee {
            Expr::Ident(name, _) if !self.has_binding(name) => {
                if super::calls::is_global_builtin(name) {
                    name.clone()
                } else {
                    return Ok(None);
                }
            }
            Expr::Member {
                object,
                property: MemberProperty::Field(method),
                ..
            } => {
                // A method of the program's own objects is not a builtin: it
                // takes the dynamic path with its receiver.
                if self.is_own_method_call(object, method) {
                    return Ok(None);
                }
                method.clone()
            }
            _ => return Ok(None),
        };
        // Distinct per call site, and spelled with a NUL no source can write.
        let site = args.as_ptr() as usize;
        let markers = (0..args.len())
            .map(|index| format!("\u{0}lash.spread-argument.{site}.{index}"))
            .collect::<Vec<_>>();
        let marked = markers
            .iter()
            .map(|marker| CallArg::Value(Expr::String(marker.clone())))
            .collect::<Vec<_>>();
        // A call this lowering cannot type with placeholder arguments is left
        // to the dynamic path, as it was before builtins took spreads.
        let mut lowered = match self.lower_call(callee, &marked) {
            Ok(lowered) => lowered,
            // A built-in method the surface refuses is refused whatever its
            // argument list; only a shape the placeholders cannot type takes
            // the dynamic path.
            Err(refusal)
                if matches!(callee, Expr::Member { .. })
                    && refusal.kind == crate::DiagnosticKind::Refusal =>
            {
                return Err(refusal);
            }
            Err(_) => return Ok(None),
        };
        let arguments = self.lower_argument_list(args)?;
        let mut arguments = Some(arguments);
        let spliced = splice_applied_arguments(&mut lowered, &markers, &mut arguments);
        if spliced == 1 && !mentions_marker(&lowered, &markers) {
            return Ok(Some(lowered));
        }
        Err(Diagnostic::refusal(
            DiagnosticCode::MethodUnsupported,
            format!(
                "Unsupported: a spread argument to `{name}`, whose lowering depends on how many arguments it takes"
            ),
            None,
        )
        .with_hint("pass the arguments one by one"))
    }
}

/// Replaces each standard-library dispatch whose trailing arguments are
/// exactly `markers`, in order, with the `Lash.Apply` form over `arguments`.
/// Returns how many it replaced.
fn splice_applied_arguments(
    expr: &mut LashExpr,
    markers: &[String],
    arguments: &mut Option<LashExpr>,
) -> usize {
    if let LashExpr::BuiltinCall { name, args } = expr
        && name.as_str() == "__typescript_stdlib"
        && args.len() > markers.len()
    {
        let fixed = args.len() - markers.len();
        let tail_is_markers = args[fixed..].iter().zip(markers).all(
            |(arg, marker)| matches!(arg, LashExpr::String(value) if value.as_str() == marker),
        );
        if tail_is_markers && let Some(arguments) = arguments.take() {
            args.truncate(fixed);
            args.insert(0, LashExpr::String("Lash.Apply".into()));
            args.push(arguments);
            return 1;
        }
    }
    expr.children_mut()
        .map(|child| splice_applied_arguments(child, markers, arguments))
        .sum()
}

fn mentions_marker(expr: &LashExpr, markers: &[String]) -> bool {
    matches!(expr, LashExpr::String(value) if markers.iter().any(|marker| value.as_str() == marker))
        || expr.children().any(|child| mentions_marker(child, markers))
}

impl Lowerer {
    /// `object.method(...args)` for a method of the program's own objects:
    /// the receiver is evaluated once, the callee is read from it, and the
    /// runtime argument list is applied with the receiver bound.
    pub(super) fn lower_method_spread_call(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        let receiver = self.temporary("method_receiver");
        let receiver_value = self.lower_expr(object)?;
        let variable = || LashExpr::Variable(receiver.as_str().into());
        let function = match property {
            MemberProperty::Field(field) => LashExpr::Field {
                target: Box::new(variable()),
                field: field.as_str().into(),
            },
            MemberProperty::Index(key) => LashExpr::Index {
                target: Box::new(variable()),
                index: Box::new(self.lower_expr(key)?),
            },
        };
        let arguments = self.lower_argument_list(args)?;
        Ok(LashExpr::Block(vec![
            LashExpr::Assign {
                target: AssignTarget::variable(receiver.as_str().into()),
                expr: Box::new(receiver_value),
            },
            LashExpr::BuiltinCall {
                name: "__typescript_call_method_dynamic".into(),
                args: vec![variable(), function, arguments],
            },
        ]))
    }
}
