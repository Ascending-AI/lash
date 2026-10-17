//! Canonical admission precedes every implementation of an operation.

use super::{Eval, KernelMachine, fault};
use crate::compile::{CodeId, Executable, LibId, LibRun};
use lash_kernel_doc::{EntityId, TaskId};
use lash_kernel_state::paged::ObjectInit;

pub(super) struct OperationEntry {
    pub(super) shared: Vec<Option<EntityId>>,
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
        self.check_call_depth(task)?;
        let function = exe.lib(lib);
        let code = match function.run {
            LibRun::Body(code)
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
                    let value = self.regs[args + index].clone();
                    shared[index] = Some(self.alloc(ObjectInit::Variable(value))?);
                }
            }
        }
        Ok(OperationEntry { shared })
    }
}
