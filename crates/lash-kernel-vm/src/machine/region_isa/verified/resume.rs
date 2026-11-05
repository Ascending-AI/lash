//! Admit semantic continuation coordinates against the current source program.
use super::super::*;
use super::{Code, control};
use crate::compile::region_isa::Role;
use crate::compile::region_isa::{
    LoopPhase, LoopStage, Pc, PrefixId, compile_region, verify_region,
};
use crate::compile::room::region::calls::region_isa::compile_calls;
use crate::machine::{Frame, controls::Controls, frames::source::SourceContinuation};
use lash_kernel_state::{kernel_internals::tree::Access, paged::FrameRecord};

fn malformed() -> HeapFault {
    HeapFault::Malformed("canonical source continuation is not admitted".into())
}

pub(super) fn position(
    code: &Code,
    source: &SourceContinuation,
) -> Result<(Pc, PrefixId), RegionError> {
    if source.root != code.program.source().site || source.progress > 4 {
        return Err(invalid(Refused::Source));
    }
    if source.progress >= 3 {
        let call = code
            .calls
            .iter()
            .flatten()
            .find(|call| {
                if source.progress == 3 {
                    call.plan.point() == &source.at
                } else {
                    call.plan.resume() == &source.at
                }
            })
            .ok_or_else(|| invalid(Refused::Source))?;
        if source.unpaid != *call.plan.point()
            || source.departure.is_some()
            || (source.progress == 3
                && (source.pending.is_some()
                    || source.debt.completion != 0
                    || source.debt.completion_paid))
        {
            return Err(invalid(Refused::Facts));
        }
        let pc = code
            .program
            .at(call.plan.point())
            .ok_or_else(|| invalid(Refused::Path))?;
        let step = code
            .program
            .step(pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        return Ok((
            pc,
            if source.progress == 3 {
                step.before
            } else {
                step.pending
            },
        ));
    }
    if !code.program.source_successor(
        &source.at,
        source.progress,
        source.departure.as_ref(),
        &source.unpaid,
    ) {
        return Err(invalid(Refused::Path));
    }
    let prefix = code
        .program
        .source_recovery(&source.at, source.progress, source.departure.as_ref())
        .ok_or_else(|| invalid(Refused::Exit))?;
    let pc = source
        .departure
        .as_ref()
        .and_then(|origin| code.program.departure_at(origin, &source.unpaid))
        .or_else(|| code.program.at(&source.unpaid))
        .ok_or_else(|| invalid(Refused::Path))?;
    Ok((pc, prefix))
}

pub(super) fn unpaid_pc(
    code: &Code,
    mut pc: Pc,
    active: impl Fn(crate::compile::StmtId) -> bool,
) -> Result<Pc, RegionError> {
    let unpaid = &code
        .program
        .step(pc)
        .ok_or_else(|| invalid(Refused::Path))?
        .at;
    let mut index = 0;
    while let Some(info) = code.program.loop_info(index) {
        if info.while_loop && *unpaid == info.entry {
            pc = code
                .program
                .stage(LoopStage {
                    info: index,
                    phase: if active(info.statement) {
                        LoopPhase::Test
                    } else {
                        LoopPhase::Enter
                    },
                })
                .ok_or_else(|| invalid(Refused::Path))?;
            break;
        }
        index += 1;
    }
    Ok(pc)
}

pub(in crate::machine) fn restore_source_controls(
    exe: &Arc<crate::compile::Executable>,
    id: CodeId,
    source: &SourceContinuation,
    record: &FrameRecord,
    access: &Access<'_>,
) -> Result<Controls, HeapFault> {
    let draft = compile_region(exe, id).map_err(|_| malformed())?;
    let calls = compile_calls(exe, &draft).map_err(|_| malformed())?;
    let program = verify_region(exe, id, draft, &calls).map_err(|_| malformed())?;
    let code = Code::bind_executable(exe, &program).map_err(|_| malformed())?;
    let mut saved = Vec::new();
    record.loops.visit(access, &mut |_, value| {
        saved.push(value.clone());
        true
    })?;
    let mut loops = Vec::new();
    let mut index = 0;
    while let Some(info) = program.loop_info(index) {
        let mut state = control::LoopState::default();
        for row in &saved {
            if row.site.resolve(access)?.as_ref() == &info.site {
                if !info.while_loop || state.active {
                    return Err(malformed());
                }
                state.active = true;
                state.started = row.started;
            }
        }
        loops.push(state);
        index += 1;
    }
    if loops.iter().filter(|state| state.active).count() != saved.len()
        || !record.finally.is_empty()
        || !record.departing.is_empty()
        || !record.iterated.is_empty()
    {
        return Err(malformed());
    }
    let (pc, prefix) = position(&code, source).map_err(|_| malformed())?;
    let roles = expected(&code, pc, prefix, source.progress == 3).map_err(|_| malformed())?;
    let expected: Vec<_> = roles
        .iter()
        .filter(|(slot, _)| {
            !matches!(
                slot,
                lash_kernel_state::kernel_internals::logical::Slot::Binding(_)
                    | lash_kernel_state::kernel_internals::logical::Slot::Capture(_)
            )
        })
        .map(|(slot, _)| *slot)
        .collect();
    if expected.len() != source.roles.len()
        || source
            .roles
            .iter()
            .any(|(slot, _)| !expected.contains(slot))
    {
        return Err(HeapFault::Malformed(format!(
            "canonical source roles differ at {:?} progress {}: expected {:?}, saved {:?}",
            source.at,
            source.progress,
            expected,
            source
                .roles
                .iter()
                .map(|(slot, _)| slot)
                .collect::<Vec<_>>()
        )));
    }
    let map = program.recovery(prefix).ok_or_else(malformed)?;
    if source.progress != 3 && map.pending.is_some() != source.pending.is_some() {
        return Err(HeapFault::Malformed(format!(
            "canonical pending result differs at {:?} progress {}: expected {}, saved {}",
            source.at,
            source.progress,
            map.pending.is_some(),
            source.pending.is_some()
        )));
    }
    control::controls_for(&code, pc, &loops).map_err(|_| malformed())
}

pub(super) fn active(frame: &Frame, stmt: crate::compile::StmtId) -> bool {
    frame.control.iter().any(|control| matches!(control, crate::machine::Control::While { stmt: at, .. } | crate::machine::Control::For { stmt: at, .. } if *at == stmt))
}

pub(super) fn reached(frame: &Frame, prefix: PrefixId) -> Result<ReachedPrefix, RegionError> {
    let source = frame.pending.source.as_ref();
    Ok(ReachedPrefix {
        program: frame.code,
        invocation: frame.invocation,
        ordinal: source.map_or(0, |source| source.ordinal),
        occurrence: source.and_then(|source| source.occurrence),
        prefix: prefix.index() as u32,
        debt: source.map_or(0, |source| source.debt.cursor),
    })
}

fn expected(
    code: &Code,
    pc: Pc,
    prefix: PrefixId,
    suspended: bool,
) -> Result<
    Vec<(
        lash_kernel_state::kernel_internals::logical::Slot,
        crate::compile::region_isa::Reg<lash_kernel_state::kernel_internals::logical::LeaseValue>,
    )>,
    RegionError,
> {
    let map = code
        .program
        .recovery(prefix)
        .ok_or_else(|| invalid(Refused::Exit))?;
    let call = if suspended {
        let crate::compile::region_isa::RegionOp::Call { site, .. } =
            code.program.op(pc).ok_or_else(|| invalid(Refused::Path))?
        else {
            return Err(invalid(Refused::Path));
        };
        Some(code.call(*site)?)
    } else {
        None
    };
    let mut values = Vec::new();
    for (slot, reg) in &map.roles {
        if let Some(call) = call {
            let physical = super::bind::physical(&code.program, *reg)?;
            if call.arguments.contains(&physical) || call.callable == Some(physical) {
                continue;
            }
            values.push((
                call.saved
                    .iter()
                    .find(|saved| saved.source == *slot)
                    .map_or(*slot, |saved| saved.parked),
                *reg,
            ));
        } else {
            values.push((*slot, *reg));
        }
    }
    Ok(values)
}

pub(super) fn values(
    code: &Code,
    frame: &Frame,
    task: lash_kernel_doc::TaskId,
    pc: Pc,
    prefix: PrefixId,
) -> Result<
    Vec<(
        crate::compile::region_isa::Reg<lash_kernel_state::kernel_internals::logical::LeaseValue>,
        Value,
    )>,
    RegionError,
> {
    use lash_kernel_state::kernel_internals::logical::Owner;
    let mut actual = std::collections::BTreeMap::new();
    for (index, state) in frame.slot_states().iter().enumerate() {
        let value = match state {
            crate::machine::SlotState::Empty => continue,
            crate::machine::SlotState::Value(value) => *value,
            crate::machine::SlotState::Cell(handle) => Value::ObjectRef(*handle),
        };
        let Some(Owner::Value { slot, .. }) =
            code.program
                .source()
                .binding_root(code.bindings[index], task, frame.invocation)
        else {
            return Err(invalid(Refused::Source));
        };
        actual.insert(slot, value);
    }
    let source = frame.pending.source.as_ref();
    if let Some(source) = source {
        actual.extend(source.roles.iter().copied());
    }
    let roles = expected(
        code,
        pc,
        prefix,
        source.is_some_and(|source| source.progress == 3),
    )?;
    if actual.len() != roles.len() {
        return Err(RegionError::Heap(HeapFault::Malformed(format!(
            "source continuation roles differ at {:?}: actual {:?}, expected {:?}",
            source.map(|source| &source.at),
            actual.keys().collect::<Vec<_>>(),
            roles.iter().map(|(slot, _)| slot).collect::<Vec<_>>()
        ))));
    }
    let mut values = roles
        .iter()
        .map(|(slot, reg)| {
            Ok((
                *reg,
                *actual.get(slot).ok_or_else(|| invalid(Refused::Facts))?,
            ))
        })
        .collect::<Result<Vec<_>, RegionError>>()?;
    if let Some(source) = source {
        if let Some(value) = source.pending {
            let reg = code
                .program
                .recovery(prefix)
                .and_then(|map| map.pending)
                .ok_or_else(|| invalid(Refused::Facts))?;
            values.push((reg, value));
        }
        if source.progress == 3 {
            for (slot, value) in &actual {
                if let Some((_, reg)) = code
                    .program
                    .roles()
                    .iter()
                    .find(|(actual, _)| *actual == Role::Slot(*slot))
                {
                    values.push((*reg, *value));
                }
            }
        }
    }
    Ok(values)
}

/// Load only the bodies demanded by the next source read, before exclusive
/// authority is acquired. No guest operation, source transfer or fee is replayed.
pub(super) fn warm_next(
    machine: &mut KernelMachine,
    code: &Code,
    frame: &Frame,
    task: lash_kernel_doc::TaskId,
    pc: Pc,
    prefix: PrefixId,
) -> Result<(), RegionError> {
    use crate::compile::region_isa::{CheckedKind, RegionOp};
    let mut needed = Vec::new();
    match code.program.op(pc).ok_or_else(|| invalid(Refused::Path))? {
        RegionOp::Call { site, .. } => {
            let call = code.call(*site)?;
            needed.extend(call.callable);
            needed.extend(call.callable_origin);
        }
        RegionOp::Checked { kernel, .. } => {
            if let CheckedKind::Local { slot } = kernel.kind()
                && let Some((_, reg)) = code
                    .program
                    .roles()
                    .iter()
                    .find(|(actual, _)| actual == slot)
            {
                needed.push(super::bind::physical(&code.program, *reg)?);
            }
        }
        RegionOp::Read { base, .. } => needed.push(super::bind::physical(&code.program, *base)?),
        _ => {}
    }
    if let Some(flow) = code.flows.get(pc.index()).and_then(Option::as_ref) {
        needed.extend(
            flow.holds
                .iter()
                .chain(&flow.releases)
                .chain(flow.quotes.iter().flatten())
                .filter(|root| root.capture)
                .map(|root| root.register),
        );
    }
    if needed.is_empty() {
        return Ok(());
    }
    for (reg, value) in values(code, frame, task, pc, prefix)? {
        if needed.contains(&super::bind::physical(&code.program, reg)?)
            && let Some(handle) = value.handle()
        {
            machine.warm_head(handle).map_err(|halt| match halt {
                crate::machine::Halt::Heap(fault) => RegionError::Heap(*fault),
                crate::machine::Halt::Fault(detail) => {
                    RegionError::Heap(HeapFault::Malformed(detail))
                }
                _ => invalid(Refused::Facts),
            })?;
            if matches!(value, Value::Closure(_)) {
                let cells = machine
                    .arena
                    .borrow()
                    .closure(handle)
                    .map_err(|error| RegionError::Heap(HeapFault::Malformed(error.to_string())))?
                    .1
                    .to_vec();
                // Entry validates each actual capture as a variable cell.
                // Load those cells before acquiring exclusive authority.
                for cell in cells {
                    machine.warm_head(cell).map_err(|halt| match halt {
                        crate::machine::Halt::Heap(fault) => RegionError::Heap(*fault),
                        crate::machine::Halt::Fault(detail) => {
                            RegionError::Heap(HeapFault::Malformed(detail))
                        }
                        _ => invalid(Refused::Facts),
                    })?;
                }
            }
        }
    }
    Ok(())
}

impl super::Running<'_> {
    pub(super) fn restore_phase(&mut self, source: SourceContinuation) -> Result<(), RegionError> {
        self.reached.ordinal = source.ordinal;
        self.reached.occurrence = source.occurrence;
        self.reached.debt = source.debt.cursor;
        self.counters.debt = source.debt.cursor;
        if let Some(origin) = source.departure.clone()
            && self.code.program.loop_origin(&origin)
        {
            if source.debt.completion != 0 || source.debt.completion_paid {
                return Err(invalid(Refused::Facts));
            }
            self.looping = Some(origin);
        } else if let Some(origin) = source.departure {
            let map = self
                .code
                .program
                .recovery(self.prefix)
                .ok_or_else(|| invalid(Refused::Exit))?;
            let reg = map
                .roles
                .iter()
                .find_map(|(slot, reg)| {
                    matches!(
                        slot,
                        lash_kernel_state::kernel_internals::logical::Slot::Departure(_)
                    )
                    .then_some(*reg)
                })
                .ok_or_else(|| invalid(Refused::Facts))?;
            let value = self.value(reg)?;
            self.departing = Some(super::returns::Departure {
                origin,
                value,
                completion: source.debt.completion,
            });
        } else if source.progress == 4 {
            let crate::compile::region_isa::RegionOp::Call { site, .. } = self
                .code
                .program
                .op(self.pc)
                .ok_or_else(|| invalid(Refused::Path))?
            else {
                return Err(invalid(Refused::Facts));
            };
            let site = site.index();
            let reg = self
                .code
                .program
                .recovery(self.prefix)
                .and_then(|map| map.pending)
                .ok_or_else(|| invalid(Refused::Facts))?;
            let value = self.value(reg)?;
            self.returning = Some(super::returns::Incoming {
                value,
                completion: source.debt.completion,
                paid: source.debt.completion_paid,
                site,
            });
        } else if source.debt.completion != 0 || source.debt.completion_paid {
            return Err(invalid(Refused::Facts));
        }
        if let Some(bound) = source.refusal {
            self.refusal = Some(match bound {
                0 => crate::Bound::Charge,
                1 => crate::Bound::Memory,
                2 => crate::Bound::CallDepth,
                _ => return Err(invalid(Refused::Facts)),
            });
            if bound == 0 {
                self.crossing = Some(super::charge::Crossing::restore(
                    self.limits.charge,
                    self.counters.charge,
                )?);
            }
            self.attempted = Some(source.unpaid);
            self.stop = Some(RegionStop::Refusal);
        }
        Ok(())
    }
}
