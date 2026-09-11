//! JavaScript unary/binary fast evaluation and async projected-operand preparation.

use super::super::{
    ensure_javascript_string_size, javascript_string_size_error, javascript_to_string,
};
use super::javascript_stdlib::js_stdlib_error;
use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn javascript_unary_needs_async(
        &self,
        op: JavaScriptUnaryOp,
    ) -> Result<bool, RuntimeError> {
        let Some(value) = self.stack.last() else {
            return Ok(true);
        };
        Ok(matches!(value, Value::Projected(_))
            || matches!(op, JavaScriptUnaryOp::Plus | JavaScriptUnaryOp::Negate)
                && self.heap.javascript_coercion_contains_projected(value)?)
    }

    pub(super) fn javascript_binary_needs_async(
        &self,
        op: JavaScriptBinaryOp,
    ) -> Result<bool, RuntimeError> {
        if self.stack.len() < 2 {
            return Ok(true);
        }
        let operands = &self.stack[self.stack.len() - 2..];
        let strict = matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        );
        Ok(operands
            .iter()
            .any(|value| matches!(value, Value::Projected(_)))
            || !strict
                && operands.iter().try_fold(false, |contains, value| {
                    if contains {
                        Ok(true)
                    } else {
                        self.heap.javascript_coercion_contains_projected(value)
                    }
                })?)
    }

    pub(super) fn execute_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<(), RuntimeError> {
        let value = self.pop_stack()?;
        debug_assert!(!matches!(value, Value::Projected(_)));
        if op == JavaScriptUnaryOp::TypeOf
            && let Value::Ref(id) = value
        {
            let kind = self.heap.get(id)?.kind_name();
            self.stack.push(Value::String(
                if kind == "function" {
                    "function"
                } else {
                    "object"
                }
                .into(),
            ));
        } else if matches!(op, JavaScriptUnaryOp::Plus | JavaScriptUnaryOp::Negate) {
            let number = self.heap.javascript_to_number(&value)?;
            self.stack
                .push(Value::Number(if op == JavaScriptUnaryOp::Negate {
                    -number
                } else {
                    number
                }));
        } else if op == JavaScriptUnaryOp::Not && matches!(value, Value::Ref(_)) {
            self.stack.push(Value::Bool(false));
        } else {
            self.stack.push(eval_javascript_unary(value, op));
        }
        Ok(())
    }

    pub(super) async fn redispatch_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<VmStep, RuntimeError> {
        let mut value = materialize_javascript_operand(self.pop_stack()?).await;
        if matches!(op, JavaScriptUnaryOp::Plus | JavaScriptUnaryOp::Negate) {
            value = self
                .heap
                .javascript_to_primitive_string_or_number_async(&value)
                .await?;
        }
        self.stack.push(value);
        self.redispatch_fast(Instruction::JavaScriptUnary(op))
    }

    pub(super) fn execute_javascript_binary(
        &mut self,
        op: JavaScriptBinaryOp,
    ) -> Result<(), RuntimeError> {
        let mut right = self.pop_stack()?;
        let mut left = self.pop_stack()?;
        debug_assert!(!matches!(left, Value::Projected(_)));
        debug_assert!(!matches!(right, Value::Projected(_)));
        let strict = matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        );
        let loose = matches!(
            op,
            JavaScriptBinaryOp::LooseEqual | JavaScriptBinaryOp::LooseNotEqual
        );
        if op == JavaScriptBinaryOp::Add
            && [&left, &right].into_iter().any(
                |value| matches!(value, Value::Ref(id) if matches!(self.heap.get(*id), Ok(HeapObject::Date(_)))),
            )
        {
            return Err(js_stdlib_error(
                "TS_DATE_STRING_COERCION_PENDING: Date addition requires unavailable host-local string semantics; use .toISOString()",
            ));
        }
        if !strict {
            let both_objects = matches!(left, Value::Ref(_)) && matches!(right, Value::Ref(_));
            if !loose || !both_objects {
                if matches!(left, Value::Ref(_)) {
                    left = self.heap.javascript_to_primitive_string_or_number(&left)?;
                }
                if matches!(right, Value::Ref(_)) {
                    right = self.heap.javascript_to_primitive_string_or_number(&right)?;
                }
            }
        }
        if op == JavaScriptBinaryOp::Add {
            let left_primitive = self.heap.javascript_to_primitive_string_or_number(&left)?;
            let right_primitive = self.heap.javascript_to_primitive_string_or_number(&right)?;
            if matches!(left_primitive, Value::String(_))
                || matches!(right_primitive, Value::String(_))
            {
                let left = javascript_to_string(&left_primitive);
                let right = javascript_to_string(&right_primitive);
                let bytes = left
                    .len()
                    .checked_add(right.len())
                    .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
                ensure_javascript_string_size(bytes)?;
                self.stack
                    .push(Value::String(format!("{left}{right}").into()));
                return Ok(());
            }
        }
        self.stack.push(eval_javascript_binary(left, op, right));
        Ok(())
    }

    pub(super) async fn prepare_javascript_binary_operands(
        &self,
        op: JavaScriptBinaryOp,
        left: Value,
        right: Value,
    ) -> Result<(Value, Value), RuntimeError> {
        let mut left = materialize_javascript_operand(left).await;
        let mut right = materialize_javascript_operand(right).await;
        let strict = matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        );
        let loose = matches!(
            op,
            JavaScriptBinaryOp::LooseEqual | JavaScriptBinaryOp::LooseNotEqual
        );
        if !strict {
            let both_references = matches!(left, Value::Ref(_)) && matches!(right, Value::Ref(_));
            if !loose || !both_references {
                if matches!(left, Value::Ref(_)) {
                    left = self
                        .heap
                        .javascript_to_primitive_string_or_number_async(&left)
                        .await?;
                }
                if matches!(right, Value::Ref(_)) {
                    right = self
                        .heap
                        .javascript_to_primitive_string_or_number_async(&right)
                        .await?;
                }
            }
        }
        if matches!(
            op,
            JavaScriptBinaryOp::Add
                | JavaScriptBinaryOp::Subtract
                | JavaScriptBinaryOp::Multiply
                | JavaScriptBinaryOp::Divide
                | JavaScriptBinaryOp::Remainder
                | JavaScriptBinaryOp::Less
                | JavaScriptBinaryOp::LessEqual
                | JavaScriptBinaryOp::Greater
                | JavaScriptBinaryOp::GreaterEqual
        ) {
            left = self
                .heap
                .javascript_to_primitive_string_or_number_async(&left)
                .await?;
            right = self
                .heap
                .javascript_to_primitive_string_or_number_async(&right)
                .await?;
        } else if loose {
            if javascript_is_object(&left) && javascript_loose_equality_coerces_object(&right) {
                left = self
                    .heap
                    .javascript_to_primitive_string_or_number_async(&left)
                    .await?;
            }
            if javascript_is_object(&right) && javascript_loose_equality_coerces_object(&left) {
                right = self
                    .heap
                    .javascript_to_primitive_string_or_number_async(&right)
                    .await?;
            }
        }
        Ok((left, right))
    }
}

async fn materialize_javascript_operand(mut value: Value) -> Value {
    while let Value::Projected(projected) = value {
        value = projected.materialize_async().await;
    }
    value
}

fn javascript_is_object(value: &Value) -> bool {
    matches!(
        value,
        Value::Image(_)
            | Value::Resource(_)
            | Value::Tuple(_)
            | Value::List(_)
            | Value::Record(_)
            | Value::Ref(_)
    )
}

fn javascript_loose_equality_coerces_object(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
}
