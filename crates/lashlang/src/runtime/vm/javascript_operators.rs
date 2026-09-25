//! JavaScript unary/binary fast evaluation and async projected-operand preparation.

use super::super::{
    ensure_javascript_string_size, javascript_string_size_error, javascript_to_string,
};
use super::*;
use crate::runtime::heap::guest_coercion::PrimitiveHint;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn javascript_unary_needs_async(
        &self,
        op: JavaScriptUnaryOp,
    ) -> Result<bool, RuntimeError> {
        let Some(value) = self.stack.last() else {
            return Ok(true);
        };
        Ok(matches!(value, Value::Projected(_))
            || (op.coerces_to_number() || op == JavaScriptUnaryOp::ToString)
                && self.heap.javascript_coercion_contains_projected(value)?)
    }

    pub(super) fn javascript_binary_needs_async(
        &self,
        op: JavaScriptBinaryOp,
    ) -> Result<bool, RuntimeError> {
        if self.stack.len() < 2 {
            return Ok(true);
        }
        let left = &self.stack[self.stack.len() - 2];
        let right = &self.stack[self.stack.len() - 1];
        if matches!(left, Value::Projected(_)) || matches!(right, Value::Projected(_)) {
            return Ok(true);
        }
        let (coerce_left, coerce_right) = javascript_binary_operand_coercions(op, left, right);
        Ok(
            (coerce_left && self.heap.javascript_coercion_contains_projected(left)?)
                || (coerce_right && self.heap.javascript_coercion_contains_projected(right)?),
        )
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
            let is_function = self.heap.get(id)?.is_function();
            self.stack.push(Value::String(
                if is_function { "function" } else { "object" }.into(),
            ));
        } else if op.coerces_to_number() {
            let number = self.heap.javascript_to_number(&value)?;
            self.stack
                .push(eval_javascript_unary(Value::Number(number), op)?);
        } else if op == JavaScriptUnaryOp::ToString {
            let text = self.heap.javascript_to_string_for_output(&value)?;
            ensure_javascript_string_size(text.len())?;
            self.stack.push(Value::String(text.into()));
        } else if op == JavaScriptUnaryOp::Not && matches!(value, Value::Ref(_)) {
            self.stack.push(Value::Bool(false));
        } else {
            self.stack.push(eval_javascript_unary(value, op)?);
        }
        Ok(())
    }

    pub(super) async fn redispatch_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<VmStep, RuntimeError> {
        let mut value = materialize_javascript_operand(self.pop_stack()?).await?;
        if op.coerces_to_number() {
            value = self
                .heap
                .javascript_to_primitive_with_hint_async(&value, PrimitiveHint::Number)
                .await?;
        } else if op == JavaScriptUnaryOp::ToString {
            value = self
                .heap
                .javascript_to_primitive_with_hint_async(&value, PrimitiveHint::String)
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
        let (coerce_left, coerce_right) = javascript_binary_operand_coercions(op, &left, &right);
        if coerce_left && matches!(left, Value::Ref(_)) {
            left = self.javascript_binary_operand_primitive(op, &left)?;
        }
        if coerce_right && matches!(right, Value::Ref(_)) {
            right = self.javascript_binary_operand_primitive(op, &right)?;
        }
        if op == JavaScriptBinaryOp::Add {
            let left_primitive = self.javascript_binary_operand_primitive(op, &left)?;
            let right_primitive = self.javascript_binary_operand_primitive(op, &right)?;
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
        let mut left = materialize_javascript_operand(left).await?;
        let mut right = materialize_javascript_operand(right).await?;
        let (coerce_left, coerce_right) = javascript_binary_operand_coercions(op, &left, &right);
        if coerce_left {
            left = self
                .javascript_binary_operand_primitive_async(op, &left)
                .await?;
        }
        if coerce_right {
            right = self
                .javascript_binary_operand_primitive_async(op, &right)
                .await?;
        }
        Ok((left, right))
    }

    /// ECMA-262 ToPrimitive on an object operand: `+` and loose equality ask
    /// the default hint; every other operator's is number — a Date's default
    /// hint is its string, so the distinction is observable.
    fn javascript_binary_operand_primitive(
        &self,
        op: JavaScriptBinaryOp,
        value: &Value,
    ) -> Result<Value, RuntimeError> {
        self.heap
            .javascript_to_primitive_with_hint(value, javascript_binary_hint(op))
    }

    async fn javascript_binary_operand_primitive_async(
        &self,
        op: JavaScriptBinaryOp,
        value: &Value,
    ) -> Result<Value, RuntimeError> {
        self.heap
            .javascript_to_primitive_with_hint_async(value, javascript_binary_hint(op))
            .await
    }
}

/// The hint ECMA-262 gives `op`'s ToPrimitive: default for `+` and loose
/// equality, number for every other operator.
fn javascript_binary_hint(op: JavaScriptBinaryOp) -> PrimitiveHint {
    match op {
        JavaScriptBinaryOp::Add
        | JavaScriptBinaryOp::LooseEqual
        | JavaScriptBinaryOp::LooseNotEqual => PrimitiveHint::Default,
        _ => PrimitiveHint::Number,
    }
}

async fn materialize_javascript_operand(mut value: Value) -> Result<Value, RuntimeError> {
    while let Value::Projected(projected) = value {
        value = projected.materialize_async().await?;
    }
    Ok(value)
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

fn javascript_binary_operand_coercions(
    op: JavaScriptBinaryOp,
    left: &Value,
    right: &Value,
) -> (bool, bool) {
    match op {
        JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual => (false, false),
        JavaScriptBinaryOp::LooseEqual | JavaScriptBinaryOp::LooseNotEqual => (
            javascript_is_object(left) && javascript_loose_equality_coerces_object(right),
            javascript_is_object(right) && javascript_loose_equality_coerces_object(left),
        ),
        _ => (javascript_is_object(left), javascript_is_object(right)),
    }
}
