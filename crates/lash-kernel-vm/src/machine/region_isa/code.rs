use super::*;
use lash_kernel_doc::scalar::ScalarOp;
use lash_kernel_state::kernel_internals::logical::Slot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Register {
    Int(u32),
    Bool(u32),
    Value(u32),
}
#[derive(Clone, Copy)]
pub(super) enum Op {
    Constant {
        dst: Register,
        value: Value,
    },
    Move {
        dst: Register,
        src: Register,
    },
    Int {
        kernel: ScalarOp,
        lhs: u32,
        rhs: u32,
        dst: Register,
    },
    Pack {
        src: Register,
        dst: Register,
    },
    Branch {
        test: u32,
        yes: u32,
        no: u32,
    },
    Read {
        base: u32,
        member: u32,
        dst: Register,
    },
    Call {
        site: u32,
    },
    Return {
        value: Register,
    },
    Throw {
        value: Register,
    },
    Boundary,
    Meter,
    Exit,
}
#[derive(Clone, Copy)]
pub(super) enum Fee {
    Fixed(u64),
    WordSize {
        value: Register,
        zero: u64,
        nonzero: u64,
    },
}
#[derive(Clone, Copy)]
pub(super) struct Cost {
    pub(super) fee: Fee,
    pub(super) pace: u64,
    pub(super) delta: i128,
    pub(super) peak: u64,
    pub(super) depth: i32,
}
impl Cost {
    pub(super) const ZERO: Self = Self {
        fee: Fee::Fixed(0),
        pace: 0,
        delta: 0,
        peak: 0,
        depth: 0,
    };
}
pub(super) struct Instruction {
    pub(super) op: Op,
    pub(super) next: u32,
    pub(super) prefix: u32,
    pub(super) cost: Cost,
}
pub(super) struct Recovery {
    pub(super) roles: Box<[(Slot, Register)]>,
    pub(super) operands: Box<[Register]>,
    pub(super) pending: Option<Register>,
    pub(super) incoming: bool,
    pub(super) continuation: Point,
    pub(super) counters: super::source_counters::CounterPlan,
}
pub(super) struct Call {
    pub(super) before: Cost,
    pub(super) before_prefix: u32,
    pub(super) entry: u32,
    pub(super) prefix: u32,
    pub(super) arguments: Box<[(Register, Register)]>,
    pub(super) result: Register,
    pub(super) destination: Register,
    pub(super) leave: Cost,
    pub(super) leave_prefix: u32,
    pub(super) completion: Cost,
    pub(super) completion_prefix: u32,
    pub(super) restore: Cost,
    pub(super) restore_prefix: u32,
}
pub(super) struct ReadDependency {
    pub(super) identity: Value,
    pub(super) members: Box<[Value]>,
}

/// Immutable execution tables. Production construction requires A's verified
/// source/phase export; only the test module constructs fixture tables.
pub(super) struct Code {
    pub(super) entry: u32,
    pub(super) entry_prefix: u32,
    pub(super) instructions: Box<[Instruction]>,
    pub(super) recovery: Box<[Recovery]>,
    pub(super) calls: Box<[Call]>,
    pub(super) dependencies: Box<[ReadDependency]>,
    pub(super) registers: usize,
    pub(super) source_initial: Box<[u64]>,
    pub(super) initial: Box<[(Register, Value)]>,
    pub(super) invocation: u64,
    pub(super) safe_integers: bool,
    #[cfg(test)]
    pub(super) semantic_operations: Arc<std::sync::atomic::AtomicU64>,
}
impl Code {
    pub(super) fn bind(
        _machine: &KernelMachine,
        code: &Arc<RegionCode>,
    ) -> Result<Arc<Self>, RegionError> {
        code.op(code.entry())
            .ok_or_else(|| invalid(Refused::Source))?;
        // Recovery alone cannot establish canonical fees or provenance.
        Err(invalid(Refused::Facts))
    }
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
