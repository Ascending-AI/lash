//! Full continuation admission for foreign checkpoint rewrites.
use super::*;

impl KernelMachine {
    pub(crate) fn validate_foreign_frames(&mut self) -> Result<(), HeapFault> {
        for index in 0..self.tasks.len() {
            let Some(task) = self.tasks.get_mut(index)? else {
                continue;
            };
            for index in 0..task.frames.len() {
                if let Some(refusal) = task.frames.refusal(index as u64)? {
                    return Err(HeapFault::Malformed(refusal.to_string()));
                }
            }
        }
        Ok(())
    }
}
