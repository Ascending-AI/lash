//! Bind immutable source-verified roles once, before acquiring state authority.
use super::super::{code::Register, *};
use crate::compile::region_isa::{Pc, Reg, RegionOp, RegisterKind};
use crate::compile::room::region::calls::region_isa::CallPlan;
use lash_kernel_state::kernel_internals::logical::Slot;
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::machine::region_isa) struct Code {
    pub(in crate::machine::region_isa) exe: Arc<crate::compile::Executable>,
    pub(in crate::machine::region_isa) arguments: usize,
    pub(super) flows: Box<[Option<super::flow::Flow>]>,
    pub(super) transfer_width: usize,
    pub(super) quotes: usize,
    pub(in crate::machine::region_isa) program: Arc<RegionCode>,
    pub(in crate::machine::region_isa) bindings: Arc<[u32]>,
    pub(in crate::machine::region_isa) calls: Box<[Option<Call>]>,
}

pub(in crate::machine::region_isa) struct Call {
    pub(in crate::machine::region_isa) plan: CallPlan,
    pub(in crate::machine::region_isa) arguments: Box<[Register]>,
    pub(in crate::machine::region_isa) saved: Box<[(Slot, Register)]>,
    pub(in crate::machine::region_isa) callable: Option<Register>,
    pub(in crate::machine::region_isa) callable_origin: Option<Register>,
    pub(in crate::machine::region_isa) result: Register,
    pub(in crate::machine::region_isa) next: Pc,
}

pub(in crate::machine::region_isa) fn physical<T>(
    code: &RegionCode,
    reg: Reg<T>,
) -> Result<Register, RegionError> {
    let index = reg.index();
    Ok(
        match code
            .registers()
            .get(index as usize)
            .ok_or_else(|| invalid(Refused::Facts))?
        {
            RegisterKind::Value => Register::Value(index),
            RegisterKind::Int => Register::Int(index),
            RegisterKind::Float => Register::Float(index),
            RegisterKind::Bool => Register::Bool(index),
        },
    )
}

impl Code {
    pub(in crate::machine::region_isa) fn bind(
        machine: &KernelMachine,
        program: &Arc<RegionCode>,
    ) -> Result<Arc<Self>, RegionError> {
        Self::bind_executable(&machine.exe, program)
    }

    pub(super) fn bind_executable(
        exe: &Arc<crate::compile::Executable>,
        program: &Arc<RegionCode>,
    ) -> Result<Arc<Self>, RegionError> {
        if program.executable() != exe.local()
            || program.code().0 as usize >= exe.code_count()
            || !Arc::ptr_eq(program.source(), &exe.code(program.code()).source_room)
        {
            return Err(invalid(Refused::Source));
        }
        let mut flows = BTreeMap::new();
        let mut arguments = 0;
        let mut calls = BTreeMap::new();
        let mut visited = BTreeSet::new();
        let mut pending = vec![program.entry()];
        while let Some(pc) = pending.pop() {
            if !visited.insert(pc.index()) {
                continue;
            }
            let step = program.step(pc).ok_or_else(|| invalid(Refused::Source))?;
            if !step.reachable {
                return Err(invalid(Refused::Source));
            }
            for prefix in [step.before, step.pending, step.after] {
                program
                    .recovery(prefix)
                    .ok_or_else(|| invalid(Refused::Exit))?;
            }
            flows.insert(pc.index(), super::flow::Flow::bind(program, pc)?);
            let op = program.op(pc).ok_or_else(|| invalid(Refused::Path))?;
            if let RegionOp::Checked { args, kernel, .. } = op {
                arguments = arguments.max(args.len());
                if let crate::compile::region_isa::CheckedKind::Native { lib, .. } = kernel.kind() {
                    arguments = arguments.max(exe.lib(*lib).arity);
                }
            }
            match op {
                RegionOp::Constant { next, .. }
                | RegionOp::Move { next, .. }
                | RegionOp::IntBinary { next, .. }
                | RegionOp::Read { next, .. }
                | RegionOp::Heap { next, .. } => pending.push(*next),
                RegionOp::Checked { kernel, next, .. } => {
                    pending.push(*next);
                    if let crate::compile::region_isa::CheckedKind::Cursor { exhausted, .. } =
                        kernel.kind()
                    {
                        pending.push(*exhausted);
                    }
                }
                RegionOp::Branch { yes, no, .. } => {
                    pending.push(*yes);
                    pending.push(*no);
                }
                RegionOp::Call { site, next } => {
                    let plan = program
                        .call(*site)
                        .ok_or_else(|| invalid(Refused::Source))?;
                    if !plan.belongs_to(exe)
                        || plan.caller() != program.code()
                        || plan.point() != &step.at
                    {
                        return Err(invalid(Refused::Source));
                    }
                    let role = |slot: Slot| -> Result<Register, RegionError> {
                        let reg = program
                            .roles()
                            .iter()
                            .find_map(|(actual, reg)| (*actual == slot).then_some(*reg))
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        physical(program, reg)
                    };
                    let call = Call {
                        plan: plan.clone(),
                        arguments: plan
                            .arguments()
                            .iter()
                            .map(|input| role(input.slot()))
                            .collect::<Result<_, _>>()?,
                        saved: plan
                            .saved_roles()
                            .iter()
                            .map(|input| Ok((input.slot(), role(input.slot())?)))
                            .collect::<Result<_, RegionError>>()?,
                        callable: plan
                            .callable()
                            .map(|input| role(input.slot()))
                            .transpose()?,
                        callable_origin: plan
                            .callable_origin()
                            .map(|input| role(input.slot()))
                            .transpose()?,
                        result: role(plan.result_role().slot())?,
                        next: *next,
                    };
                    if calls.insert(site.index() as usize, call).is_some() {
                        return Err(invalid(Refused::Source));
                    }
                    pending.push(*next);
                }
                RegionOp::Boundary { .. } | RegionOp::Return { .. } | RegionOp::Exit { .. } => {}
            }
        }
        let count = calls.last_key_value().map_or(Ok(0), |(&index, _)| {
            index.checked_add(1).ok_or_else(|| invalid(Refused::Facts))
        })?;
        let mut bound = Vec::new();
        bound
            .try_reserve_exact(count)
            .map_err(|_| invalid(Refused::Scratch))?;
        bound.resize_with(count, || None);
        for (index, call) in calls {
            bound[index] = Some(call);
        }
        let width = visited
            .last()
            .copied()
            .and_then(|last| last.checked_add(1))
            .ok_or_else(|| invalid(Refused::Facts))?;
        let transfer_width = flows
            .values()
            .map(super::flow::Flow::width)
            .max()
            .unwrap_or(0);
        let quotes = flows
            .values()
            .map(|flow| flow.quotes.len())
            .max()
            .unwrap_or(0);
        let mut flow_table = Vec::new();
        flow_table
            .try_reserve_exact(width)
            .map_err(|_| invalid(Refused::Scratch))?;
        flow_table.resize_with(width, || None);
        for (index, flow) in flows {
            flow_table[index] = Some(flow);
        }
        Ok(Arc::new(Self {
            exe: Arc::clone(exe),
            arguments,
            flows: flow_table.into(),
            transfer_width,
            quotes,
            program: Arc::clone(program),
            calls: bound.into_boxed_slice(),
            bindings: Arc::clone(&exe.code(program.code()).slots_by_at),
        }))
    }

    pub(in crate::machine::region_isa) fn call(&self, site: CallId) -> Result<&Call, RegionError> {
        self.calls
            .get(site.index() as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid(Refused::Source))
    }
}
