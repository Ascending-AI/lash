use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    pub(super) fn pop_stack(&mut self) -> Result<Value, RuntimeError> {
        self.stack.pop().ok_or(RuntimeError::VmStackUnderflow)
    }

    pub(super) fn load_slot(&self, slot: usize) -> Result<&Value, RuntimeError> {
        self.slots
            .get(slot)
            .ok_or_else(|| RuntimeError::UndefinedVariable {
                name: slot_names_for(self.chunk, self.active_function)[slot]
                    .text
                    .to_string(),
            })
    }

    pub(super) fn drain_record_from_stack(&mut self, keys: usize) -> Result<Record, RuntimeError> {
        let key_indices = &self.chunk.key_lists[keys];
        let start = self.stack_drain_start(key_indices.len())?;
        let mut record = record_with_capacity(key_indices.len());
        for (key, value) in key_indices.iter().zip(self.stack.drain(start..)) {
            let name_entry = &self.chunk.names[*key];
            record.insert_symbolized(name_entry.symbol, name_entry.text.clone(), value);
        }
        Ok(record)
    }

    pub(super) fn drain_receiver_call(
        &mut self,
        argc: usize,
    ) -> Result<(Value, Vec<Value>), RuntimeError> {
        let start = self.stack_drain_start(argc + 1)?;
        let mut values = self.stack.drain(start..).collect::<Vec<_>>();
        let receiver = values.remove(0);
        Ok((receiver, values))
    }

    pub(super) fn pop_n(&mut self, len: usize) -> Result<Vec<Value>, RuntimeError> {
        if self.stack.len() < len {
            return Err(RuntimeError::VmStackUnderflow);
        }
        let start = self.stack.len() - len;
        Ok(self.stack.split_off(start))
    }

    pub(super) fn stack_tail(&self, len: usize) -> Result<&[Value], RuntimeError> {
        if self.stack.len() < len {
            return Err(RuntimeError::VmStackUnderflow);
        }
        Ok(&self.stack[self.stack.len() - len..])
    }

    pub(super) fn stack_drain_start(&self, len: usize) -> Result<usize, RuntimeError> {
        self.stack
            .len()
            .checked_sub(len)
            .ok_or(RuntimeError::VmStackUnderflow)
    }
}
