//! The exclusive register bank pins the exact entry values through acceptance.
use super::*;
use lash_kernel_state::kernel_internals::logical::Owner;

pub(in crate::machine) struct PreparedEntryBank<'a> {
    registers: &'a mut Registers,
    inputs: source::EntryInputs,
}
impl PreparedEntryBank<'_> {
    pub(in crate::machine) fn stage_export(
        self,
        start: usize,
        callee: Option<lash_kernel_exec::Reg>,
        control: bool,
    ) -> Result<(), HeapFault> {
        self.registers
            .source
            .stage_export(start, callee, control, self.inputs)
    }
    pub(in crate::machine) fn public_arguments(&self) -> Option<&[(Option<Owner>, Value)]> {
        self.inputs.public_arguments()
    }

    pub(in crate::machine) fn arguments(
        &self,
    ) -> impl Iterator<Item = (Option<Owner>, Value)> + '_ {
        self.inputs.arguments()
    }
    pub(in crate::machine) fn owners(&self) -> impl Iterator<Item = Owner> + '_ {
        self.inputs.owners()
    }
    pub(in crate::machine) fn call_owners(&self) -> impl Iterator<Item = Owner> + '_ {
        self.inputs.call_owners()
    }
    pub(in crate::machine) fn dependencies(
        &self,
    ) -> impl Iterator<Item = lash_kernel_doc::arena::ArenaHandle> + '_ {
        self.inputs
            .snapshots()
            .filter_map(|(_, value)| value.handle())
    }
    pub(in crate::machine) fn saved(
        &self,
    ) -> impl Iterator<
        Item = (
            &crate::compile::Suffix,
            Value,
            &lash_kernel_state::kernel_internals::logical::Holding,
            Owner,
            Owner,
        ),
    > + '_ {
        self.inputs.saved()
    }
    pub(in crate::machine) fn publish(self) {
        for (reg, _) in self.inputs.snapshots() {
            self.registers.take(reg.ix(), Value::Absent);
        }
        self.registers.source.publish_entry(self.inputs);
    }
}
impl Registers {
    pub(in crate::machine) fn finish_export_arguments(
        &mut self,
        start: usize,
        count: usize,
    ) -> Result<(), HeapFault> {
        self.source
            .finish_export(start, self.values[start..start + count].to_vec())
    }
    pub(in crate::machine) fn public_entry_count(
        &self,
        start: usize,
        count: usize,
    ) -> Result<usize, HeapFault> {
        self.source.public_entry_count(start, count)
    }

    pub(in crate::machine) fn prepare_frame_entry(
        &mut self,
        start: usize,
        count: usize,
        callee: Option<lash_kernel_exec::Reg>,
        control: bool,
        roles: &[crate::compile::NestedRole],
    ) -> Result<Option<PreparedEntryBank<'_>>, HeapFault> {
        let Some(inputs) = self.source.prepare_entry(
            start,
            count,
            callee,
            control,
            &self.values[..self.extent],
            roles,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(PreparedEntryBank {
            registers: self,
            inputs,
        }))
    }
}
