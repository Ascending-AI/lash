use super::{Expr, MethodKey, Printer, TypeScriptSourceError, stdlib_call};

impl Printer<'_> {
    pub(super) fn property_presence(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Some((receiver, key)) = own_property_parts(expression) else {
            return Ok(None);
        };
        let receiver = if matches!(receiver, Expr::Record(_)) {
            format!("({})", self.expression(receiver)?)
        } else {
            self.member_target(receiver)?
        };
        Ok(Some(format!(
            "{receiver}.hasOwnProperty({})",
            self.expression(key)?
        )))
    }
}

fn own_property_parts(expression: &Expr) -> Option<(&Expr, &Expr)> {
    if let Some([receiver, key]) = stdlib_call(expression, "Object.hasOwn")
        && (matches!(
            receiver,
            Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::List(_)
        ) || stdlib_call(receiver, "Lash.SparseArray").is_some())
    {
        return Some((receiver, key));
    }
    let Expr::Block(items) = expression else {
        return None;
    };
    let [
        Expr::Assign {
            target,
            expr: receiver,
        },
        Expr::If {
            condition,
            then_block,
            else_block,
        },
    ] = items.as_slice()
    else {
        return None;
    };
    if !target.is_simple() {
        return None;
    }
    let [Expr::Variable(checked), Expr::String(method)] = stdlib_call(condition, "Lash.OwnMethod")?
    else {
        return None;
    };
    let Expr::MethodCall {
        receiver: own_receiver,
        method: MethodKey::Field(own_method),
        args,
    } = then_block.as_ref()
    else {
        return None;
    };
    let [key] = args.as_slice() else {
        return None;
    };
    let [Expr::Variable(fallback_receiver), fallback_key] =
        stdlib_call(else_block, "Object.hasOwn")?
    else {
        return None;
    };
    (checked == &target.root
        && fallback_receiver == &target.root
        && method.as_str() == "hasOwnProperty"
        && own_method.as_str() == "hasOwnProperty"
        && matches!(own_receiver.as_ref(), Expr::Variable(name) if name == &target.root)
        && serde_json::to_vec(key).ok()? == serde_json::to_vec(fallback_key).ok()?)
    .then_some((receiver.as_ref(), key))
}
