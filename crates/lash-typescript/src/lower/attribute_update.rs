//! The compound attribute update: the one place a rendered graph rebuilds it.
//!
//! `object.step op= operand` lowers to a [`StructuralRole::AttributeAssign`]
//! whose value applies `op` to the attribute read through the pinned base. A
//! graph records only the target, the operator and the operand, so rendering
//! it back rebuilds that role here; the printer reads it back as `op=`.

use lashlang::{AssignPathStep, AssignTarget, Expr, StructuralRole, UpdateOperator};

use super::GENERATED_BINDING_PREFIX;

/// The role `target op= operand` lowers to, or `None` when `target` is not a
/// one-step member target.
pub(crate) fn attribute_update(
    target: &AssignTarget,
    operator: UpdateOperator,
    operand: Expr,
) -> Option<Expr> {
    let [step] = target.steps.as_slice() else {
        return None;
    };
    let base = format!("{GENERATED_BINDING_PREFIX}update_base");
    let key = format!("{GENERATED_BINDING_PREFIX}update_key");
    let result = format!("{GENERATED_BINDING_PREFIX}update_result");
    let variable = |name: &str| Expr::Variable(name.into());
    let assign = |name: &str, expr: Expr| Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(expr),
    };
    let mut block = vec![assign(&base, Expr::Variable(target.root.clone()))];
    let (current, store_step) = match step {
        AssignPathStep::Field(field) => (
            Expr::Field {
                target: Box::new(variable(&base)),
                field: field.clone(),
            },
            AssignPathStep::Field(field.clone()),
        ),
        AssignPathStep::Index(index) => {
            block.push(assign(&key, index.clone()));
            (
                Expr::Index {
                    target: Box::new(variable(&base)),
                    index: Box::new(variable(&key)),
                },
                AssignPathStep::Index(variable(&key)),
            )
        }
    };
    block.push(assign(
        &result,
        Expr::JavaScriptBinary {
            left: Box::new(current),
            op: operator.javascript_op(),
            right: Box::new(operand),
        },
    ));
    block.push(Expr::Assign {
        target: AssignTarget {
            root: base.as_str().into(),
            steps: vec![store_step],
        },
        expr: Box::new(variable(&result)),
    });
    block.push(variable(&result));
    Some(Expr::Role {
        role: StructuralRole::AttributeAssign,
        expr: Box::new(Expr::Block(block)),
    })
}
