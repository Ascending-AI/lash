//! Canonical admission precedes every implementation of an operation.

use super::{Eval, KernelMachine};
use crate::compile::CodeId;
use lash_kernel_doc::TaskId;

pub(super) struct OperationEntry {
    pub(super) depth_headroom: u32,
}

pub(super) struct AdmittedBody {
    pub(super) code: CodeId,
    pub(super) entry: Option<OperationEntry>,
}

impl From<CodeId> for AdmittedBody {
    fn from(code: CodeId) -> Self {
        Self { code, entry: None }
    }
}

impl KernelMachine {
    /// A template may elide nested helper frames only with sufficient
    /// depth headroom. On a miss the normal path names the exact refusal.
    #[inline]
    pub(super) fn template_depth_fits(
        &mut self,
        task: TaskId,
        extra: u32,
        entry: Option<&OperationEntry>,
    ) -> Eval<bool> {
        if extra == 0 {
            return Ok(true);
        }
        if let Some(entry) = entry {
            return Ok(extra <= entry.depth_headroom);
        }
        let depth =
            self.task(task)?.frames.len() + usize::from(task != TaskId::MAIN || !self.session_cell);
        Ok(depth as u64 + u64::from(extra) <= u64::from(self.bounds.call_depth))
    }

    /// Admits the operation's depth unit before selecting an implementation.
    /// Shared parameter storage belongs to the body's prepared source entry.
    pub(super) fn operation_entry(&mut self, task: TaskId) -> Eval<OperationEntry> {
        let (_, depth_headroom) = self.call_depth_headroom(task)?;
        Ok(OperationEntry { depth_headroom })
    }
}
