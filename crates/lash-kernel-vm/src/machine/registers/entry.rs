//! The exclusive register bank pins the exact entry values through acceptance.
use super::*;
use lash_kernel_state::kernel_internals::logical::Owner;

pub(in crate::machine) struct PreparedEntryBank<'a> {
    registers: &'a mut Registers,
    inputs: source::EntryInputs,
}
impl PreparedEntryBank<'_> {
    pub(in crate::machine) fn arguments(
        &self,
    ) -> impl Iterator<Item = (Option<Owner>, Value)> + '_ {
        self.inputs.arguments()
    }
    pub(in crate::machine) fn owners(&self) -> impl Iterator<Item = Owner> + '_ {
        self.inputs.owners()
    }
    pub(in crate::machine) fn publish(self) {
        for (reg, _) in self.inputs.snapshots() {
            self.registers.take(reg.ix(), Value::Absent);
        }
        self.registers.source.publish_entry(self.inputs);
    }
}
impl Registers {
    pub(in crate::machine) fn prepare_frame_entry(
        &mut self,
        start: usize,
        count: usize,
        callee: Option<lash_kernel_exec::Reg>,
        control: bool,
    ) -> Result<Option<PreparedEntryBank<'_>>, HeapFault> {
        let Some(inputs) = self.source.prepare_entry(
            start,
            count,
            callee,
            control,
            &self.values[..self.extent],
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
