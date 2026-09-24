//! Lowering for array literals.
//!
//! An all-value literal is the dense `List` it reads as. An elision has no
//! expression of its own: the literal still lowers dense, and the hole's
//! index is recorded beside it so `indexOf`, `lastIndexOf` and `in` can tell
//! an absent slot from a stored `undefined`.

use lashlang::{AssignTarget, Expr as LashExpr};

use super::Lowerer;
use crate::Diagnostic;
use crate::adapter::ArrayElement;

impl Lowerer {
    pub(super) fn lower_array_literal(
        &mut self,
        elements: &[ArrayElement],
    ) -> Result<LashExpr, Diagnostic> {
        if elements
            .iter()
            .all(|element| matches!(element, ArrayElement::Value(_)))
        {
            return Ok(LashExpr::List(
                elements
                    .iter()
                    .map(|element| match element {
                        ArrayElement::Value(value) => self.lower_expr(value),
                        _ => unreachable!(),
                    })
                    .collect::<Result<_, _>>()?,
            ));
        }
        let has_holes = elements
            .iter()
            .any(|element| matches!(element, ArrayElement::Hole));
        let result = self.temporary("array_spread");
        let holes = self.temporary("array_holes");
        let mut expressions = vec![temp_assignment(&result, LashExpr::List(Vec::new()))];
        if has_holes {
            expressions.push(temp_assignment(&holes, LashExpr::List(Vec::new())));
        }
        for element in elements {
            // A hole lands at the list's current length, which a preceding
            // spread can only say at run time, so the index is recorded as it
            // is appended rather than counted statically.
            if matches!(element, ArrayElement::Hole) {
                expressions.push(temp_assignment(
                    &holes,
                    stdlib_call(
                        "concat",
                        vec![
                            variable(&holes),
                            LashExpr::List(vec![LashExpr::Field {
                                target: Box::new(variable(&result)),
                                field: "length".into(),
                            }]),
                        ],
                    ),
                ));
            }
            let next = match element {
                ArrayElement::Value(value) => LashExpr::List(vec![self.lower_expr(value)?]),
                ArrayElement::Hole => LashExpr::List(vec![LashExpr::Undefined]),
                ArrayElement::Spread(value) => {
                    let value = self.lower_iterable_sink(value)?;
                    Self::iterable_copy(value)
                }
            };
            expressions.push(temp_assignment(
                &result,
                stdlib_call("concat", vec![variable(&result), next]),
            ));
        }
        expressions.push(if has_holes {
            stdlib_call(
                "Lash.SparseArray",
                vec![variable(&result), variable(&holes)],
            )
        } else {
            variable(&result)
        });
        Ok(LashExpr::Block(expressions))
    }
}

fn variable(name: &str) -> LashExpr {
    LashExpr::Variable(name.into())
}

fn temp_assignment(name: &str, value: LashExpr) -> LashExpr {
    LashExpr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(value),
    }
}

fn stdlib_call(method: &str, args: Vec<LashExpr>) -> LashExpr {
    let mut values = vec![LashExpr::String(method.into())];
    values.extend(args);
    LashExpr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args: values,
    }
}
