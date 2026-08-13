use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn read_dialect_field(
        &mut self,
        target: Value,
        field: &Name,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_field(&self.heap, id, field);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_field_direct(target, field);
        }
        if self.reference_semantics {
            read_javascript_field_direct(target, field)
        } else {
            read_field_direct(target, field)
        }
    }

    pub(super) fn read_dialect_index(
        &mut self,
        target: Value,
        index: Value,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_index(&self.heap, id, &index);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_index_direct(target, index);
        }
        if self.reference_semantics {
            read_javascript_index_direct(target, index)
        } else {
            read_index_direct(target, index)
        }
    }

    pub(super) fn execute_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<(), RuntimeError> {
        let mut value = self.pop_stack()?;
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
        } else {
            if op != JavaScriptUnaryOp::TypeOf && matches!(value, Value::Ref(_)) {
                value = self.heap.export_for_instruction(&value)?;
            }
            self.stack.push(eval_javascript_unary(value, op));
        }
        Ok(())
    }

    pub(super) fn execute_javascript_binary(
        &mut self,
        op: JavaScriptBinaryOp,
    ) -> Result<(), RuntimeError> {
        let mut right = self.pop_stack()?;
        let mut left = self.pop_stack()?;
        if !matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        ) {
            if matches!(left, Value::Ref(_)) {
                left = self.heap.export_for_instruction(&left)?;
            }
            if matches!(right, Value::Ref(_)) {
                right = self.heap.export_for_instruction(&right)?;
            }
        }
        self.stack.push(eval_javascript_binary(left, op, right));
        Ok(())
    }

    pub(super) fn execute_javascript_split(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        let values = javascript_split(&value, &separator)?;
        self.stack.push(self.heap.allocate_list(values)?);
        Ok(())
    }

    pub(super) fn execute_javascript_join(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        self.stack
            .push(Value::String(javascript_join(&value, &separator)?.into()));
        Ok(())
    }
}
