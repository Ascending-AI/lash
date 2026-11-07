//! Bind immutable source-verified roles once, before acquiring state authority.
use super::super::{code::Register, *};
use crate::compile::region_isa::Role;
use crate::compile::region_isa::{Pc, Reg, RegionOp, RegisterKind};
use crate::compile::room::region::calls::region_isa::CallPlan;
use lash_kernel_state::kernel_internals::logical::Slot;
use std::collections::{BTreeMap, BTreeSet};

pub(in crate::machine::region_isa) struct Code {
    pub(in crate::machine::region_isa) exe: Arc<crate::compile::Executable>,
    pub(in crate::machine::region_isa) arguments: usize,
    pub(super) captures: usize,
    pub(super) flows: Box<[Option<super::flow::Flow>]>,
    pub(super) transfer_width: usize,
    pub(super) quotes: usize,
    pub(in crate::machine::region_isa) program: Arc<RegionCode>,
    pub(in crate::machine::region_isa) bindings: Arc<[u32]>,
    pub(in crate::machine::region_isa) calls: Box<[Option<Call>]>,
    /// The session cells the code names, by lease-wide bank cell.
    pub(super) sessions: Box<[(u32, SessionCell)]>,
}

/// A durable session cell: its logical owner digest and its name.
#[derive(Clone)]
pub(super) struct SessionCell {
    pub(super) digest: [u8; 32],
    pub(super) name: crate::compile::BindingName,
}

/// The ordinal among the region's session roles of the one `register`
/// holds. Roles are sorted and numbered in order, so a role's register is
/// its index and the session roles are contiguous.
fn session_index(code: &RegionCode, register: u32) -> Option<usize> {
    let roles = code.roles();
    let index = usize::try_from(register).ok()?;
    let session = |(role, _): &(Role, _)| matches!(role, Role::Session(_));
    match roles.get(index) {
        Some(entry @ (_, reg)) if session(entry) && reg.index() == register => Some(
            roles[..index]
                .iter()
                .rev()
                .take_while(|entry| session(entry))
                .count(),
        ),
        _ => None,
    }
}

pub(super) struct SavedRole {
    pub(super) source: Slot,
    pub(super) parked: Slot,
    pub(super) suffix: Arc<[u32]>,
    pub(super) register: Register,
}

pub(in crate::machine::region_isa) struct Call {
    pub(in crate::machine::region_isa) plan: CallPlan,
    pub(in crate::machine::region_isa) arguments: Box<[Register]>,
    /// A statement call's atom loads; its arguments stay held by the caller.
    pub(super) atoms: Box<[crate::compile::region_isa::AtomLoad]>,
    pub(super) saved: Box<[SavedRole]>,
    pub(super) counter: source_counters::CounterPlan,
    pub(in crate::machine::region_isa) callable: Option<Register>,
    pub(in crate::machine::region_isa) callable_origin: Option<Register>,
    pub(in crate::machine::region_isa) next: Pc,
}

pub(in crate::machine::region_isa) fn physical<T>(
    code: &RegionCode,
    reg: Reg<T>,
) -> Result<Register, RegionError> {
    physical_index(code, reg.index())
}

/// The physical register of `code`'s register `index`: a session role's
/// lease-wide bank cell, else the frame register of its kind.
pub(super) fn physical_index(code: &RegionCode, index: u32) -> Result<Register, RegionError> {
    if let Some(ordinal) = session_index(code, index) {
        return code
            .session_banks()
            .get(ordinal)
            .map(|&bank| Register::Session(bank))
            .ok_or_else(|| invalid(Refused::Facts));
    }
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

/// Each code's bound tables, bound once per machine: binding reads only the
/// machine's executable, which never changes, and the code's verified
/// region, which the executable keeps.
#[derive(Default)]
pub(in crate::machine) struct BoundCodes(std::sync::Mutex<BTreeMap<CodeId, Arc<Code>>>);

impl BoundCodes {
    pub(in crate::machine::region_isa) fn get(
        &self,
        exe: &Arc<crate::compile::Executable>,
        program: &Arc<RegionCode>,
    ) -> Result<Arc<Code>, RegionError> {
        let mut bound = self.0.lock().map_err(|_| invalid(Refused::Facts))?;
        if let Some(code) = bound.get(&program.code())
            && Arc::ptr_eq(&code.exe, exe)
            && Arc::ptr_eq(&code.program, program)
        {
            return Ok(Arc::clone(code));
        }
        let code = Code::bind_executable(exe, program)?;
        bound.insert(program.code(), Arc::clone(&code));
        Ok(code)
    }
}

impl Code {
    pub(in crate::machine::region_isa) fn bind(
        machine: &KernelMachine,
        program: &Arc<RegionCode>,
    ) -> Result<Arc<Self>, RegionError> {
        machine.bound.get(&machine.exe, program)
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
        let mut captures = 0;
        let mut calls = BTreeMap::new();
        let mut visited = BTreeSet::new();
        let mut pending = std::iter::once(program.entry())
            .chain(program.raise_origins())
            .collect::<Vec<_>>();
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
            if let RegionOp::Heap { args, kernel, .. } = op {
                arguments = arguments.max(args.len());
                if let crate::compile::region_isa::HeapKind::Closure {
                    captures: slots, ..
                } = kernel.kind()
                {
                    captures = captures.max(slots.len());
                }
            }
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
                    match kernel.kind() {
                        crate::compile::region_isa::CheckedKind::Cursor { exhausted, .. } => {
                            pending.push(*exhausted);
                        }
                        crate::compile::region_isa::CheckedKind::Header {
                            driver, ended, ..
                        } => {
                            pending.extend(*driver);
                            pending.extend(*ended);
                        }
                        crate::compile::region_isa::CheckedKind::Cleanup { at } => {
                            pending.extend(program.restores(at));
                        }
                        _ => {}
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
                    let register = |role: Role| -> Result<Register, RegionError> {
                        let reg = program
                            .roles()
                            .iter()
                            .find_map(|(actual, reg)| (*actual == role).then_some(*reg))
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        physical(program, reg)
                    };
                    let role = |slot: Slot| register(Role::Slot(slot));
                    let call = Call {
                        plan: plan.clone(),
                        counter: match plan.occurrence() {
                            Some(index) => {
                                let index =
                                    u32::try_from(index).map_err(|_| invalid(Refused::Facts))?;
                                source_counters::CounterPlan::bind(
                                    Box::new([source_counters::CounterEdit { index, advance: 1 }]),
                                    Some(source_counters::CounterRead::Issued(index)),
                                    exe.occurrence_count(),
                                )?
                            }
                            None => source_counters::CounterPlan::empty(),
                        },
                        arguments: plan
                            .arguments()
                            .iter()
                            .map(|input| role(input.slot()))
                            .collect::<Result<_, _>>()?,
                        atoms: program
                            .call_atoms(*site)
                            .ok_or_else(|| invalid(Refused::Source))?
                            .into(),
                        saved: saved_roles(exe, program, plan)?,
                        callable: plan.callable().map(register).transpose()?,
                        callable_origin: plan.callable_origin().map(register).transpose()?,
                        next: *next,
                    };
                    arguments = arguments.max(call.arguments.len());
                    if calls.insert(site.index() as usize, call).is_some() {
                        return Err(invalid(Refused::Source));
                    }
                    pending.push(*next);
                }
                RegionOp::Return { .. } => {
                    pending.push(
                        program
                            .return_successor(pc)
                            .ok_or_else(|| invalid(Refused::Path))?,
                    );
                }
                RegionOp::Boundary { spec } => {
                    if spec.action().is_some() {
                        pending.push(spec.next().ok_or_else(|| invalid(Refused::Facts))?);
                    }
                }
                RegionOp::Effect { next, .. } => pending.extend(*next),
                RegionOp::Raise { .. } | RegionOp::ReturnVia { .. } | RegionOp::Exit { .. } => {}
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
        let slots = program.source().owner_slots().map_err(invalid)?;
        let sessions = program
            .roles()
            .iter()
            .filter_map(|(role, _)| match role {
                Role::Session(digest) => Some(*digest),
                Role::Slot(_) => None,
            })
            .zip(program.session_banks())
            .map(|(digest, &bank)| {
                Ok((
                    bank,
                    SessionCell {
                        digest,
                        name: crate::compile::BindingName::new(
                            slots
                                .session_name(&digest)
                                .ok_or_else(|| invalid(Refused::Facts))?
                                .clone(),
                        ),
                    },
                ))
            })
            .collect::<Result<_, RegionError>>()?;
        Ok(Arc::new(Self {
            sessions,
            exe: Arc::clone(exe),
            arguments,
            captures,
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

fn saved_roles(
    exe: &crate::compile::Executable,
    program: &RegionCode,
    plan: &CallPlan,
) -> Result<Box<[SavedRole]>, RegionError> {
    if plan.saved_roles().is_empty() {
        return Ok(Box::new([]));
    }
    let mut statement = plan.point().site.clone();
    while statement.path.len() > program.source().site.path.len()
        && exe.stmt_at(program.code(), &statement).is_none()
    {
        statement.path.pop();
    }
    let stmt = exe
        .stmt_at(program.code(), &statement)
        .ok_or_else(|| invalid(Refused::Source))?;
    let (stream, _, _) = exe.statement(stmt);
    let act = stream
        .nested_at
        .get(&(stmt, plan.point().site.clone()))
        .ok_or_else(|| invalid(Refused::Source))?;
    let nested = &stream.nested[act.ix()];
    if nested.roles.len() != plan.saved_roles().len() {
        return Err(invalid(Refused::Source));
    }
    let slots = program.source().owner_slots().map_err(invalid)?;
    nested
        .roles
        .iter()
        .zip(plan.saved_roles())
        .map(|(role, saved)| {
            use lash_kernel_state::kernel_internals::logical::Owner;
            let Owner::Value { slot: source, .. } = slots
                .root(
                    &crate::compile::room::Owner::Operand(role.site.clone()),
                    lash_kernel_doc::TaskId::MAIN,
                    0,
                )
                .ok_or_else(|| invalid(Refused::Source))?
            else {
                return Err(invalid(Refused::Source));
            };
            let Owner::Value { slot: parked, .. } = slots
                .continuation(&role.site, lash_kernel_doc::TaskId::MAIN, 0)
                .ok_or_else(|| invalid(Refused::Source))?
            else {
                return Err(invalid(Refused::Source));
            };
            if saved.slot() != source {
                return Err(invalid(Refused::Source));
            }
            let reg = program
                .roles()
                .iter()
                .find_map(|(slot, reg)| (*slot == Role::Slot(source)).then_some(*reg))
                .ok_or_else(|| invalid(Refused::Facts))?;
            Ok(SavedRole {
                source,
                parked,
                suffix: Arc::clone(&role.suffix),
                register: physical(program, reg)?,
            })
        })
        .collect()
}
