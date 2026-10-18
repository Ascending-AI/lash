//! Canonical admission precedes every implementation of an operation.

use super::{Eval, KernelMachine, fault};
use crate::compile::{CodeId, Executable, LibId, LibRun};
use lash_kernel_doc::TaskId;
use lash_kernel_doc::arena::ArenaHandle;

pub(super) struct OperationEntry {
    pub(super) shared: Vec<Option<ArenaHandle>>,
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
        let depth = self.task(task)?.frames.len()
            + self.plans as usize
            + usize::from(task != TaskId::MAIN || !self.session_cell);
        Ok(depth as u64 + u64::from(extra) <= u64::from(self.bounds.call_depth))
    }

    /// Admits a call of an operation before any implementation is offered
    /// it: the call's depth unit (K-BND-002) and the cells of its shared
    /// parameters, so their identities are the same whichever
    /// implementation answers. Memory is checked where it is allocated, as
    /// a conservative bound (K-BND-003); admission reserves none ahead.
    pub(super) fn operation_entry(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: usize,
    ) -> Eval<OperationEntry> {
        let depth_headroom = self.call_depth_headroom(task)?;
        let function = exe.lib(lib);
        let code = match function.run {
            LibRun::Body(code)
            | LibRun::Plan { body: code, .. }
            | LibRun::Native {
                body: Some(code), ..
            } => exe.code(code),
            _ => return Err(fault("an operation has no body").into()),
        };
        let mut shared = Vec::new();
        if code.params.iter().any(|param| param.shared) {
            shared.resize(function.arity, None);
            for (index, param) in code.params.iter().enumerate() {
                if param.shared {
                    let value = self.regs[args + index];
                    shared[index] = Some(self.new_cell(value)?);
                }
            }
        }
        Ok(OperationEntry {
            shared,
            depth_headroom,
        })
    }
}
