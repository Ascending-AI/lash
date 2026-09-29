use super::super::{
    StringValue, ensure_javascript_string_size, javascript_operand_text,
    javascript_string_size_error,
};
use super::javascript_operators::javascript_binary_operand_coercions;
use super::*;

// In-place assignment opcode: the fused `+=` store. It writes a durable
// binding, so it is an isolation boundary: the value that lands in the slot is
// exclusively owned by it.

impl<H: ExecutionHost> Vm<'_, H> {
    /// `s = s + rhs` under ECMA-262 `+` rules (FIG-3733).
    ///
    /// The operand stack carries `JavaScriptBinary`'s pair — the accumulator
    /// the `LoadName` compiled in front of the right operand pushed — so
    /// evaluation order, the projection gate and the guest-coercion replay
    /// are exactly the unfused `LoadName; JavaScriptBinary; StoreName`
    /// sequence's; only the store is fused. When the accumulator's buffer is
    /// uniquely owned the append reuses it — `StringValue`'s copy-on-write
    /// keeps a binding that aliases it (`const t = s`) on the old bytes —
    /// which takes releasing the primitive's, `last_value`'s and the slot's
    /// own shares before `push_str`.
    pub(super) fn javascript_add_assign(&mut self, slot: usize) -> Result<(), RuntimeError> {
        let mut right = self.pop_stack()?;
        let mut left = self.pop_stack()?;
        debug_assert!(!matches!(left, Value::Projected(_)));
        debug_assert!(!matches!(right, Value::Projected(_)));
        let (coerce_left, coerce_right) =
            javascript_binary_operand_coercions(JavaScriptBinaryOp::Add, &left, &right);
        if coerce_left && matches!(left, Value::Ref(_)) {
            left = self.javascript_binary_operand_primitive(JavaScriptBinaryOp::Add, &left)?;
        }
        if coerce_right && matches!(right, Value::Ref(_)) {
            right = self.javascript_binary_operand_primitive(JavaScriptBinaryOp::Add, &right)?;
        }
        let left_primitive =
            self.javascript_binary_operand_primitive(JavaScriptBinaryOp::Add, &left)?;
        let right_primitive =
            self.javascript_binary_operand_primitive(JavaScriptBinaryOp::Add, &right)?;
        let value = if matches!(left_primitive, Value::String(_))
            || matches!(right_primitive, Value::String(_))
        {
            let right = javascript_operand_text(&right_primitive);
            match left {
                // A string operand's primitive is that same string: releasing
                // `left_primitive`, the previous completion and the slot's own
                // share leaves `accumulator` sole owner, so `push_str`'s
                // copy-on-write extends the buffer in place. The slot is
                // cleared rather than overwritten only after assignability —
                // and the size cap, which an unfused `JavaScriptBinary` would
                // raise first — have settled, so a failing store leaves the
                // binding intact.
                Value::String(mut accumulator) if matches!(left_primitive, Value::String(_)) => {
                    let bytes = accumulator
                        .len()
                        .checked_add(right.len())
                        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
                    ensure_javascript_string_size(bytes)?;
                    // Concatenating writes the whole result once.
                    self.charge_intrinsic_work(bytes);
                    self.slots.ensure_assignable(
                        slot,
                        slot_names_for(self.chunk, self.active_function),
                        self.active_projected_bindings(),
                    )?;
                    drop(left_primitive);
                    self.last_value = None;
                    if let Some(slot_value) = self.slots.values.get_mut(slot) {
                        *slot_value = None;
                    }
                    accumulator.push_str(&right);
                    Value::String(accumulator)
                }
                _ => {
                    let left = javascript_operand_text(&left_primitive);
                    let bytes = left
                        .len()
                        .checked_add(right.len())
                        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
                    ensure_javascript_string_size(bytes)?;
                    // Concatenating writes the whole result once.
                    self.charge_intrinsic_work(bytes);
                    Value::String(StringValue::concatenated(&left, &right))
                }
            }
        } else {
            eval_javascript_binary(left, JavaScriptBinaryOp::Add, right)
        };
        // `StoreName`'s order: the operands' values settle, then assignability
        // resolves and the slot takes the result.
        self.slots.assign(
            slot,
            value.clone(),
            slot_names_for(self.chunk, self.active_function),
            self.active_function
                .is_none()
                .then_some(&self.projected_bindings),
        )?;
        self.last_value = Some(value);
        Ok(())
    }
}
