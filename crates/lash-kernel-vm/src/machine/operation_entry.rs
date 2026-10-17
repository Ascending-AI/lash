//! Canonical admission precedes every implementation of an operation.

use super::{Eval, KernelMachine, fault};
use crate::compile::{CodeId, Executable, LibId, LibRun};
use crate::heap::value_bytes;
use lash_kernel_doc::{EntityId, TaskId, Value};
use lash_kernel_state::paged::ObjectInit;

pub(super) struct OperationEntry {
    pub(super) pinned: u64,
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
        // Admission prices the current semantic roots before adding the
        // callee's holdings; lazy root publication must not choose a plan's
        // memory verdict.
        self.publish_holdings()?;
        let registers = u64::from(code.registers).saturating_mul(value_bytes(&Value::Null));
        let mut bytes = super::FRAME_BYTES.saturating_add(registers);
        let mut pinned = registers + super::FRAME_BYTES;
        for (index, param) in code.params.iter().enumerate() {
            let cost = value_bytes(&self.regs[args + index]);
            // The body retains its original arguments for its formula, as
            // well as binding each parameter. A shared binding owns a cell.
            bytes = bytes.saturating_add(cost);
            if param.shared {
                bytes = bytes.saturating_add(32);
            } else {
                pinned = pinned.saturating_add(cost);
            }
        }
        self.reserve_held(bytes)?;
        let mut shared = Vec::new();
        if code.params.iter().any(|param| param.shared) {
            shared.resize(function.arity, None);
            for (index, param) in code.params.iter().enumerate() {
                if param.shared {
                    let value = self.regs[args + index].clone();
                    let cost = 32u64.saturating_add(value_bytes(&value));
                    self.heap.transfer_held_to_reservation(cost)?;
                    shared[index] = Some(self.alloc(ObjectInit::Variable(value))?);
                }
            }
        }
        // The argument registers serve as the native's parameter holdings.
        // Its shadow holds the other argument copy. The complete prepaid
        // register file also covers the slots emptied during body handoff.
        self.pins.bytes = self
            .pins
            .bytes
            .checked_add(pinned)
            .ok_or_else(|| fault("operation holding overflow"))?;
        self.heap.execution_roots_moved();
        Ok(OperationEntry { pinned, shared })
    }

    pub(super) fn release_operation(&mut self, entry: OperationEntry) {
        self.pins.bytes -= entry.pinned;
        self.heap.execution_roots_moved();
    }
}

/// Guard answers live in the prepaid register file. Kind names are held by
/// the executable's immutable pool; boolean answers are immediates.
pub(super) fn guard_immediate(value: &Value) -> bool {
    match value {
        Value::Bool(_) => true,
        Value::Tuple(tuple) if tuple.summary.len == 0 => true,
        Value::Text(text) => match &text.body {
            lash_kernel_doc::heap::Payload::Resident(text) => matches!(
                text.as_ref(),
                "null"
                    | "absent"
                    | "bool"
                    | "integer"
                    | "float"
                    | "text"
                    | "bytes"
                    | "timestamp"
                    | "tuple"
                    | "list"
                    | "map"
                    | "set"
                    | "record"
                    | "closure"
                    | "error"
                    | "task"
                    | "function"
                    | "handle"
                    | "ref"
            ),
            _ => false,
        },
        _ => false,
    }
}
