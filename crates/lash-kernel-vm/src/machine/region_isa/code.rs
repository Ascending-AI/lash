use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Register {
    Int(u32),
    Float(u32),
    Bool(u32),
    Value(u32),
    /// A session cell's lease-wide bank cell: one per digest, shared by
    /// every frame and outside any frame's bank.
    Session(u32),
}
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) charge: u64,
    pub(super) memory: u64,
    pub(super) depth: u32,
    pub(super) slice: u64,
    pub(super) entry_balance: u64,
    pub(super) entry_charge: u64,
    pub(super) entry_progress: u64,
    pub(super) charge_enabled: bool,
    pub(super) entry_depth: u32,
}
impl Limits {
    pub(super) fn machine(machine: &mut KernelMachine) -> Result<Self, RegionError> {
        let task = machine.current.ok_or_else(|| invalid(Refused::Facts))?;
        let below = usize::from(task != lash_kernel_doc::TaskId::MAIN || !machine.session_cell);
        let frames = machine
            .tasks
            .get_mut(task.0 as usize)
            .map_err(RegionError::Heap)?
            .ok_or_else(|| invalid(Refused::Facts))?
            .frames
            .len();
        let entry_depth = frames
            .checked_add(below)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid(Refused::Facts))?;
        Ok(Self {
            charge: if matches!(machine.bounds.charge, crate::Charge::Off) {
                u64::MAX
            } else {
                machine.bounds.charge_limit
            },
            memory: if matches!(machine.bounds.memory_meter, crate::MemoryMeter::Off) {
                u64::MAX
            } else {
                machine.bounds.memory
            },
            depth: machine.bounds.call_depth,
            slice: machine.slice_end,
            entry_balance: machine.logical_memory.get(),
            entry_charge: if matches!(machine.bounds.charge, crate::Charge::Off) {
                0
            } else {
                machine.charged
            },
            entry_progress: machine.charged,
            charge_enabled: !matches!(machine.bounds.charge, crate::Charge::Off),
            entry_depth,
        })
    }
}
