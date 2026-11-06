//! An uncaught raise leaves the region from its frame's departure role.
use super::*;
use crate::machine::{Completion, Halt, Interrupt, fault};
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::Slot;

impl KernelMachine {
    /// `task`'s top frame stands after a raise the region accepted that no
    /// try of the frame takes, its value in the frame's departure role. The
    /// machine carries it out of the frame, ending each frame it leaves, as
    /// any raise that frame cannot take; it is born in this step.
    pub(super) fn raise_from_region(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
    ) -> Result<(), Halt> {
        let value = self
            .frame(task)?
            .pending
            .source
            .as_ref()
            .ok_or_else(|| fault("an uncaught raise has no source continuation"))?
            .roles
            .iter()
            .find_map(|(slot, value)| matches!(slot, Slot::Departure(_)).then_some(*value))
            .ok_or_else(|| fault("an uncaught raise holds no departure"))?;
        self.raised = None;
        let exe = Arc::clone(&self.exe);
        self.complete(task, host, &exe, Completion::Throw(Box::new(value)))
    }

    /// A step the machine served for `task`'s top frame raised: an action,
    /// its delivery, a native answer or an effect's copy. The raise is born
    /// where the frame stands and leaves each frame no try of it takes; one
    /// a canonical frame's try would take faults in `complete`.
    pub(super) fn raise_out(
        &mut self,
        task: TaskId,
        host: &mut dyn crate::interface::Host,
        interrupt: Interrupt,
    ) -> Result<bool, Halt> {
        self.regs.clear();
        self.raised = None;
        let value = self.thrown_by(task, interrupt)?;
        let exe = Arc::clone(&self.exe);
        self.complete(task, host, &exe, Completion::Throw(value))?;
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            self.pins.clear();
        }
        Ok(true)
    }
}
