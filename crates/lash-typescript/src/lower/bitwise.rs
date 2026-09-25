use super::*;

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

impl Lowerer {
    pub(super) fn lower_bit_not(&mut self, value: &Expr) -> Result<LashExpr, Diagnostic> {
        let lowered = self.lower_expr(value)?;
        let input = self.temporary("bit_not");
        let value = self.to_uint32(variable(&input));
        Ok(LashExpr::Block(vec![
            temp_assignment(&input, lowered),
            self.to_int32(js_subtract(LashExpr::Number(4_294_967_295.0), value)),
        ]))
    }

    fn to_uint32(&self, value: LashExpr) -> LashExpr {
        let finite = stdlib_call("Number.isFinite", vec![value.clone()]);
        let truncated = stdlib_call("Math.trunc", vec![value]);
        let modulo = LashExpr::JavaScriptBinary {
            left: Box::new(truncated),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(4_294_967_296.0)),
        };
        let positive = LashExpr::JavaScriptBinary {
            left: Box::new(js_add(modulo, LashExpr::Number(4_294_967_296.0))),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(4_294_967_296.0)),
        };
        LashExpr::If {
            condition: Box::new(finite),
            then_block: Box::new(positive),
            else_block: Box::new(LashExpr::Number(0.0)),
        }
    }

    fn to_int32(&self, value: LashExpr) -> LashExpr {
        LashExpr::If {
            condition: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(value.clone()),
                op: JavaScriptBinaryOp::GreaterEqual,
                right: Box::new(LashExpr::Number(2_147_483_648.0)),
            }),
            then_block: Box::new(js_subtract(
                value.clone(),
                LashExpr::Number(4_294_967_296.0),
            )),
            else_block: Box::new(value),
        }
    }

    fn bit_at(value: LashExpr, power: f64) -> LashExpr {
        let quotient = stdlib_call(
            "Math.floor",
            vec![LashExpr::JavaScriptBinary {
                left: Box::new(value),
                op: JavaScriptBinaryOp::Divide,
                right: Box::new(LashExpr::Number(power)),
            }],
        );
        LashExpr::JavaScriptBinary {
            left: Box::new(quotient),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(2.0)),
        }
    }

    fn balanced_sum(mut values: Vec<LashExpr>) -> LashExpr {
        while values.len() > 1 {
            values = values
                .chunks(2)
                .map(|chunk| match chunk {
                    [left, right] => js_add(left.clone(), right.clone()),
                    [value] => value.clone(),
                    _ => unreachable!(),
                })
                .collect();
        }
        values.pop().unwrap_or(LashExpr::Number(0.0))
    }

    pub(super) fn lower_bitwise_pair(
        &mut self,
        left: LashExpr,
        op: BinaryOp,
        right: LashExpr,
    ) -> LashExpr {
        let left = self.to_uint32(left);
        let right = self.to_uint32(right);
        let bits = (0..32)
            .map(|index| {
                let power = 2_f64.powi(index);
                let left_bit = Self::bit_at(left.clone(), power);
                let right_bit = Self::bit_at(right.clone(), power);
                let bit = match op {
                    BinaryOp::BitAnd => LashExpr::JavaScriptBinary {
                        left: Box::new(left_bit),
                        op: JavaScriptBinaryOp::Multiply,
                        right: Box::new(right_bit),
                    },
                    BinaryOp::BitOr => LashExpr::If {
                        condition: Box::new(js_add(left_bit, right_bit)),
                        then_block: Box::new(LashExpr::Number(1.0)),
                        else_block: Box::new(LashExpr::Number(0.0)),
                    },
                    BinaryOp::BitXor => LashExpr::JavaScriptBinary {
                        left: Box::new(js_add(left_bit, right_bit)),
                        op: JavaScriptBinaryOp::Remainder,
                        right: Box::new(LashExpr::Number(2.0)),
                    },
                    _ => unreachable!(),
                };
                LashExpr::JavaScriptBinary {
                    left: Box::new(bit),
                    op: JavaScriptBinaryOp::Multiply,
                    right: Box::new(LashExpr::Number(power)),
                }
            })
            .collect();
        self.to_int32(Self::balanced_sum(bits))
    }

    pub(super) fn lower_shift(
        &mut self,
        left: LashExpr,
        op: BinaryOp,
        right: LashExpr,
    ) -> LashExpr {
        let left = self.to_uint32(left);
        let shift = LashExpr::JavaScriptBinary {
            left: Box::new(self.to_uint32(right)),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(32.0)),
        };
        let factor = stdlib_call("Math.pow", vec![LashExpr::Number(2.0), shift]);
        match op {
            BinaryOp::ShiftLeft => self.to_int32(LashExpr::JavaScriptBinary {
                left: Box::new(LashExpr::JavaScriptBinary {
                    left: Box::new(left),
                    op: JavaScriptBinaryOp::Multiply,
                    right: Box::new(factor),
                }),
                op: JavaScriptBinaryOp::Remainder,
                right: Box::new(LashExpr::Number(4_294_967_296.0)),
            }),
            BinaryOp::ShiftRightUnsigned => stdlib_call(
                "Math.floor",
                vec![LashExpr::JavaScriptBinary {
                    left: Box::new(left),
                    op: JavaScriptBinaryOp::Divide,
                    right: Box::new(factor),
                }],
            ),
            BinaryOp::ShiftRight => stdlib_call(
                "Math.floor",
                vec![LashExpr::JavaScriptBinary {
                    left: Box::new(self.to_int32(left)),
                    op: JavaScriptBinaryOp::Divide,
                    right: Box::new(factor),
                }],
            ),
            _ => unreachable!(),
        }
    }
}
