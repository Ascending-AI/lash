// In-place assignment opcodes: compound `+=` forms and list appends. Each of
// these writes a durable binding, so each is an isolation boundary: the value
// that lands in the slot is exclusively owned by it.

impl<H: ExecutionHost> Vm<'_, H> {
    fn append_assign(&mut self, slot: usize) -> Result<(), RuntimeError> {

            let item = self.pop_stack()?;
            self.slots.ensure_assignable(slot, &self.chunk.slot_names)?;
            // `xs = xs + [item]` appends into the accumulator's own object.
            // Every other holder of the old value already owns a separate
            // copy, so the append is unobservable outside this binding.
            let heap_target = match self.slots.get(slot) {
                Some(Value::Ref(id))
                    if matches!(self.heap.get(*id), Ok(HeapObject::List(_))) =>
                {
                    Some(Value::Ref(*id))
                }
                _ => None,
            };
            if let Some(target) = heap_target {
                let value = self.heap.push_list(&target, item)?;
                self.record_assignment(slot);
                self.last_value = Some(value);
                return Ok(());
            }
            self.materialize_mutable_slot(slot)?;
            let slot_name = &self.chunk.slot_names[slot];
            let current =
                self.slots
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
            self.record_assignment(slot);
            self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    fn add_assign_value(&mut self, slot: usize, right: Value) -> Result<(), RuntimeError> {
        self.slots.ensure_assignable(slot, &self.chunk.slot_names)?;
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
            self.record_assignment(slot);
            self.last_value = Some(value);
            return Ok(());
        }
        self.materialize_mutable_slot(slot)?;
        let right = self.heap.export_for_instruction(&right)?;
        let slot_name = &self.chunk.slot_names[slot];
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
        self.record_assignment(slot);
        self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    fn add_assign_number(&mut self, slot: usize, right: f64) -> Result<(), RuntimeError> {
        let slot_name = &self.chunk.slot_names[slot];
        self.slots.ensure_assignable(slot, &self.chunk.slot_names)?;
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
                left => {
                    let value = add_values(left.clone(), Value::Number(right))?;
                    *left = value.clone();
                    value
                }
            }
        };
        self.record_assignment(slot);
        self.last_value = Some(value);
        Ok(())
    }

    #[inline(always)]
    fn add_assign_slot(&mut self, slot: usize, right: usize) -> Result<(), RuntimeError> {
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
    fn add_assign_index_number(
        &mut self,
        slot: usize,
        index: &Value,
        right: f64,
    ) -> Result<(), RuntimeError> {
        let slot_name = &self.chunk.slot_names[slot];
        self.slots.ensure_assignable(slot, &self.chunk.slot_names)?;
        let root = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| RuntimeError::UndefinedVariable {
                name: slot_name.text.to_string(),
            })?;
        let value = add_assign_index_number(root, index, right)?;
        self.record_assignment(slot);
        self.last_value = Some(value);
        Ok(())
    }
}
