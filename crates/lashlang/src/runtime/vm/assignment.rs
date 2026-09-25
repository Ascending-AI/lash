use super::super::{
    StringValue, ensure_javascript_string_size, javascript_operand_text,
    javascript_string_size_error, stringify_value_blocking,
};
use super::javascript_operators::javascript_binary_operand_coercions;
use super::*;

// In-place assignment opcodes: compound `+=` forms and list appends. Each of
// these writes a durable binding, so each is an isolation boundary: the value
// that lands in the slot is exclusively owned by it.

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

    pub(super) fn append_assign(&mut self, slot: usize) -> Result<(), RuntimeError> {
        let item = self.pop_stack()?;
        self.slots.ensure_assignable(
            slot,
            slot_names_for(self.chunk, self.active_function),
            self.active_projected_bindings(),
        )?;
        // `xs = xs + [item]` appends into the accumulator's own object.
        // Every other holder of the old value already owns a separate
        // copy, so the append is unobservable outside this binding.
        let heap_target = match self.slots.get(slot) {
            Some(Value::Ref(id)) if matches!(self.heap.get(*id), Ok(HeapObject::List(_))) => {
                Some(Value::Ref(*id))
            }
            _ => None,
        };
        if let Some(target) = heap_target {
            let value = self.heap.push_list(&target, item)?;
            self.last_value = Some(value);
            return Ok(());
        }
        self.materialize_mutable_slot(slot)?;
        let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
        let current = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| RuntimeError::UndefinedVariable {
                name: slot_name.text.to_string(),
            })?;
        let value = if let Value::List(items) = current {
            let values = items.make_mut();
            if values.len() == values.capacity() {
                values.reserve(1);
            }
            values.push(item);
            Value::List(items.clone())
        } else {
            add_values(current.clone(), Value::List(vec![item].into()))?
        };
        self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    pub(super) fn add_assign_value(
        &mut self,
        slot: usize,
        right: Value,
    ) -> Result<(), RuntimeError> {
        self.slots.ensure_assignable(
            slot,
            slot_names_for(self.chunk, self.active_function),
            self.active_projected_bindings(),
        )?;
        // A list accumulator grows in place. Every other holder of its old
        // value already owns a separate copy, so extending the object it names
        // is unobservable — and it costs what is being appended rather than
        // what has been accumulated.
        let extend_target = match self.slots.get(slot) {
            Some(Value::Ref(id))
                if matches!(self.heap.get(*id), Ok(HeapObject::List(_)))
                    && self.heap.is_list(&right) =>
            {
                Some(Value::Ref(*id))
            }
            _ => None,
        };
        if let Some(target) = extend_target {
            let value = self.heap.extend_list(&target, &right)?;
            self.last_value = Some(value);
            return Ok(());
        }
        self.materialize_mutable_slot(slot)?;
        let right = self.heap.export_for_instruction(&right)?;
        let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
        let value = {
            let left = self
                .slots
                .get_mut(slot)
                .ok_or_else(|| RuntimeError::UndefinedVariable {
                    name: slot_name.text.to_string(),
                })?;
            match (left, right) {
                (Value::Number(left), Value::Number(right)) => {
                    *left += right;
                    Value::Number(*left)
                }
                (Value::String(accumulator), Value::String(right)) => {
                    // Same amortization as `JavaScriptAddAssign`: release the
                    // completion's share so a uniquely owned buffer appends
                    // in place; a shared one copies on write.
                    self.last_value = None;
                    accumulator.push_str(right.as_str());
                    Value::String(accumulator.clone())
                }
                (left, right) => {
                    let value = add_values(left.clone(), right)?;
                    *left = value.clone();
                    value
                }
            }
        };
        // Concatenation copies the operands' members into a new container, so
        // the container that lands in the slot is isolated before it is stored.
        let value = if matches!(
            value,
            Value::Tuple(_) | Value::List(_) | Value::Record(_) | Value::Ref(_)
        ) {
            let isolated = self.heap.isolate_value(&value)?;
            self.slots.values[slot] = Some(isolated.clone());
            isolated
        } else {
            value
        };
        self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn add_assign_number(
        &mut self,
        slot: usize,
        right: f64,
    ) -> Result<(), RuntimeError> {
        let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
        self.slots.ensure_assignable(
            slot,
            slot_names_for(self.chunk, self.active_function),
            self.active_projected_bindings(),
        )?;
        let value = {
            let left = self
                .slots
                .get_mut(slot)
                .ok_or_else(|| RuntimeError::UndefinedVariable {
                    name: slot_name.text.to_string(),
                })?;
            match left {
                Value::Number(left) => {
                    *left += right;
                    Value::Number(*left)
                }
                Value::String(accumulator) => {
                    let text = stringify_value_blocking(&Value::Number(right))?;
                    self.last_value = None;
                    accumulator.push_str(&text);
                    Value::String(accumulator.clone())
                }
                left => {
                    let value = add_values(left.clone(), Value::Number(right))?;
                    *left = value.clone();
                    value
                }
            }
        };
        self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn add_assign_slot(
        &mut self,
        slot: usize,
        right: usize,
    ) -> Result<(), RuntimeError> {
        // The number fast path needs both sides to already be numbers: neither
        // slot is exported for this opcode, so a heap-backed accumulator has to
        // go the long way round rather than be asked for its numeric value.
        if let (Some(Value::Number(_)), Value::Number(right)) =
            (self.slots.get(slot), self.load_slot(right)?)
        {
            let right = *right;
            return self.add_assign_number(slot, right);
        }
        let right = self.load_slot(right)?.clone();
        self.add_assign_value(slot, right)
    }

    #[inline(always)]
    pub(super) fn add_assign_index_number(
        &mut self,
        slot: usize,
        index: &Value,
        right: f64,
    ) -> Result<(), RuntimeError> {
        let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
        self.slots.ensure_assignable(
            slot,
            slot_names_for(self.chunk, self.active_function),
            self.active_projected_bindings(),
        )?;
        let root = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| RuntimeError::UndefinedVariable {
                name: slot_name.text.to_string(),
            })?;
        let value = add_assign_index_number(root, index, right)?;
        self.last_value = Some(value);
        Ok(())
    }
}
