use super::*;

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
        if elements
            .iter()
            .all(|element| matches!(element, ArrayElement::Value(_) | ArrayElement::Hole))
        {
            // Elisions lower to a dense list plus the hole positions: the
            // `Lash.SparseArray` builtin marks them so `in`,
            // `hasOwnProperty` and `sort` see ECMA's absent slots rather than
            // stored `undefined`s.
            let mut values = Vec::with_capacity(elements.len());
            let mut holes = Vec::new();
            for (index, element) in elements.iter().enumerate() {
                match element {
                    ArrayElement::Value(value) => values.push(self.lower_expr(value)?),
                    ArrayElement::Hole => {
                        holes.push(LashExpr::Number(index as f64));
                        values.push(LashExpr::Undefined);
                    }
                    ArrayElement::Spread(_) => unreachable!(),
                }
            }
            return Ok(Self::stdlib_call(
                "Lash.SparseArray",
                vec![LashExpr::List(values), LashExpr::List(holes)],
            ));
        }
        let result = self.temporary("array_spread");
        let mut expressions = vec![Self::temp_assignment(&result, LashExpr::List(Vec::new()))];
        for element in elements {
            let next = match element {
                ArrayElement::Value(value) => LashExpr::List(vec![self.lower_expr(value)?]),
                // A hole beside a spread densifies to a stored `undefined` —
                // the concat path has no hole channel.
                ArrayElement::Hole => LashExpr::List(vec![LashExpr::Undefined]),
                ArrayElement::Spread(value) => {
                    let value = self.lower_iterable_sink(value)?;
                    Self::iterable_copy(value)
                }
            };
            expressions.push(Self::temp_assignment(
                &result,
                Self::stdlib_call("concat", vec![Self::variable(&result), next]),
            ));
        }
        expressions.push(Self::variable(&result));
        Ok(LashExpr::Block(expressions))
    }
}
