//! The `collection_transform` role's TypeScript spelling: a method call whose
//! receiver, callback and operands the role's shape names.

use lash_vm::{Expr, StructuralRole};

use super::{Printer, TypeScriptSourceError, stdlib_call};

impl<'p> Printer<'p> {
    /// A collection-transform role prints back as `receiver.<operation>(fn,
    /// ..)`: its operands are the authored arguments after the callback — a
    /// `reduce` initial value, a predicate's `thisArg`, the excess arguments
    /// ECMAScript evaluates and ignores — in their authored order.
    ///
    /// `Array.from(source, fn, ..)` lowers to the same role with operation
    /// `arrayFromMap` and an iteration-source receiver, so its spelling is
    /// the static call; `toSorted`'s copy step is a role operand that is not
    /// an authored argument and is dropped.
    pub(super) fn collection_transform(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Expr::Role {
            role: StructuralRole::CollectionTransform { operation },
            expr,
        } = expression
        else {
            return Ok(None);
        };
        let Some(parts) = lash_vm::CollectionTransformParts::of(expr) else {
            return Ok(None);
        };
        let mut operands = parts.operands;
        // `toSorted` sorts a copy of the receiver: the role's last operand
        // rebinds the receiver slot to its `slice`, an implementation step
        // with no authored argument.
        if operation.as_str() == "toSorted"
            && let Some(Expr::Assign { target, expr }) = operands.last()
            && target.is_simple()
            && let Some([Expr::Variable(copy)]) = stdlib_call(expr, "slice")
            && copy == &target.root
        {
            operands = &operands[..operands.len() - 1];
        }
        // Each operand is a generated slot's initialization; the value it
        // binds is the authored argument. The guarded shape re-lowers the
        // arguments for its own-method arm, so its generated names differ
        // from the setup's — the role's shape, not a name comparison, is the
        // authority.
        let mut arguments = Vec::with_capacity(operands.len() + 1);
        for operand in operands {
            let Expr::Assign { expr: value, .. } = operand else {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a collection transform operand that is not an argument",
                });
            };
            arguments.push(self.expression(value)?);
        }
        let callback = self.expression(parts.callback)?;
        if operation.as_str() == "arrayFromMap" {
            // `Array.from(source, fn, ..)`: the role's receiver is the
            // iteration source the source argument lowers to.
            let Some([source]) = stdlib_call(parts.receiver, "Lash.ArrayIterationSource") else {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "an Array.from map over a non-iteration source",
                });
            };
            let mut all = vec![self.expression(source)?, callback];
            all.extend(arguments);
            return Ok(Some(format!("Array.from({})", all.join(", "))));
        }
        let mut all = vec![callback];
        all.extend(arguments);
        Ok(Some(format!(
            "{}.{}({})",
            self.member_target(parts.receiver)?,
            self.identifier("operation", operation.as_str())?,
            all.join(", ")
        )))
    }
}
