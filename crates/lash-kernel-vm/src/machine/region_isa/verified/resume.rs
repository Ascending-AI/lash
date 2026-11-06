//! Admit semantic continuation coordinates against the current source program.
use super::super::*;
use super::{Code, control};
use crate::compile::region_isa::Role;
use crate::compile::region_isa::{LoopPhase, LoopStage, Pc, PrefixId};
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
        // A suspended caller stands after its call's Enter: what the call
        // moved is gone, what it holds is live.
        return Ok((
            pc,
            if source.progress == 3 {
                step.after
            } else {
                step.pending
            },
        ));
    }
    let held: Vec<_> = source.roles.iter().map(|(slot, _)| *slot).collect();
    if !code.program.source_successor(
        &source.at,
        source.progress,
        source.departure.as_ref(),
        &held,
        &source.unpaid,
    ) {
        return Err(invalid(Refused::Path));
    }
    let prefix = code
        .program
        .source_recovery(
            &source.at,
            source.progress,
            source.departure.as_ref(),
            &held,
            &source.unpaid,
        )
        .ok_or_else(|| invalid(Refused::Exit))?;
    let pc = code
        .program
        .source_next(
            &source.at,
            source.progress,
            source.departure.as_ref(),
            &held,
            &source.unpaid,
        )
        .ok_or_else(|| invalid(Refused::Path))?;
    Ok((pc, prefix))
}

/// The delivering action a frame stands at, its Enter accepted and its
/// result not yet delivered: the action its origin names, the Commit it
/// resumes at, and its arguments' values.
pub(in crate::machine) struct Standing {
    pub(in crate::machine) statement: crate::compile::StmtId,
    /// The nested action, or `None` for the statement's own action.
    pub(in crate::machine) act: Option<lash_kernel_exec::ActIx>,
    pub(in crate::machine) resume: crate::compile::room::Point,
    pub(in crate::machine) arguments: Vec<Value>,
}

/// A frame stands at an action once its Enter is accepted.
pub(in crate::machine) fn standing(
    code: &Code,
    source: &SourceContinuation,
) -> Result<Option<Standing>, RegionError> {
    use crate::compile::region_isa::RegionOp;
    if source.progress != 2 || source.departure.is_some() {
        return Ok(None);
    }
    let Some(pc) = code.program.at(&source.at) else {
        return Ok(None);
    };
    let Some(RegionOp::Boundary { spec }) = code.program.op(pc) else {
        return Ok(None);
    };
    if spec.action().is_none() {
        return Ok(None);
    }
    position(code, source)?;
    let origin = spec.origin().ok_or_else(|| invalid(Refused::Facts))?;
    let act = match origin.op {
        lash_kernel_exec::Op::Nested { act, .. } => Some(act),
        lash_kernel_exec::Op::Act => None,
        _ => return Err(invalid(Refused::Source)),
    };
    let (slots, commit) = code
        .program
        .action_arguments(pc)
        .ok_or_else(|| invalid(Refused::Facts))?;
    let arguments = slots
        .iter()
        .map(|slot| {
            source
                .roles
                .iter()
                .find_map(|(saved, value)| (saved == slot).then_some(*value))
                .ok_or_else(|| invalid(Refused::Facts))
        })
        .collect::<Result<_, _>>()?;
    Ok(Some(Standing {
        statement: origin.statement,
        act,
        resume: code
            .program
            .step(commit)
            .ok_or_else(|| invalid(Refused::Path))?
            .at
            .clone(),
        arguments,
    }))
}

/// The event a raise served to a frame's next step is born at: a standing
/// action's Commit, the Restore of the call a suspended frame awaits or a
/// returned call stands at, or else the step the frame stands before (a
/// native's answer, a checked call, a refused condition).
pub(in crate::machine) fn raised_at(
    code: &Code,
    source: &SourceContinuation,
) -> Result<Option<crate::compile::room::Point>, RegionError> {
    if source.departure.is_some() {
        return Ok(None);
    }
    Ok(match source.progress {
        // Before its step, or after the step before it.
        0 => Some(hooked(code, &source.at)),
        2 => Some(match standing(code, source)? {
            Some(standing) => standing.resume,
            None => hooked(code, &source.unpaid),
        }),
        3 => code
            .calls
            .iter()
            .flatten()
            .find(|call| call.plan.point() == &source.at)
            .map(|call| call.plan.resume().clone()),
        4 => Some(source.at.clone()),
        _ => None,
    })
}

/// A protocol `for`'s header row that refuses what a hook returned raises
/// at that hook's call, as the hook's own raise does (`K-ITER-005`).
fn hooked(code: &Code, at: &crate::compile::room::Point) -> crate::compile::room::Point {
    use crate::compile::room::Phase;
    let via = code
        .program
        .loop_at(&at.site)
        .and_then(|index| code.program.loop_info(index))
        .is_some_and(|info| info.via.is_some());
    let phase = match at.phase {
        Phase::Restore if via => Phase::Open,
        Phase::Fallthrough if via => Phase::Next,
        Phase::Supersede if via => Phase::Check,
        phase => phase,
    };
    crate::compile::room::Point {
        site: at.site.clone(),
        phase,
    }
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
        let stepped = info.while_loop
            || matches!(
                code.exe.stmt(info.statement).form,
                crate::compile::Form::For { via: None, .. }
            );
        if stepped && *unpaid == info.entry {
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
    departing: &[Value],
    access: &Access<'_>,
) -> Result<(Controls, Option<Standing>), HeapFault> {
    let program = exe.region(id).map_err(|_| malformed())?;
    let code = Code::bind_executable(exe, &program).map_err(|_| malformed())?;
    let mut saved = Vec::new();
    record.loops.visit(access, &mut |_, value| {
        saved.push(value.clone());
        true
    })?;
    let mut loops = Vec::new();
    // Each loop the point stands in is active at the passes it started, at
    // its cursor's position and with its guard; where its header stands
    // follows from the point.
    let mut index = 0;
    while let Some(info) = program.loop_info(index) {
        let mut state = control::LoopState::default();
        for row in &saved {
            if row.site.resolve(access)?.as_ref() == &info.site {
                if state.active {
                    return Err(malformed());
                }
                state.active = true;
                state.started = row.started;
                if let Some(cursor) = row.cursor {
                    match cursor.kind {
                        // A table cursor saves the sequence it last took,
                        // `MAX` for none.
                        lash_kernel_state::paged::CursorKind::Table => {
                            state.last = (cursor.position != u64::MAX).then_some(cursor.position);
                        }
                        lash_kernel_state::paged::CursorKind::Driver => {}
                        _ => state.position = cursor.position,
                    }
                    state.guard = cursor.guard.map(|guard| super::call_bind::GuardArguments {
                        size: guard.size,
                        remaining: guard.remaining,
                        more: row.header != lash_kernel_state::paged::HeaderRecord::Exhausted,
                    });
                }
            }
        }
        loops.push(state);
        index += 1;
    }
    if loops.iter().filter(|state| state.active).count() != saved.len() {
        return Err(malformed());
    }
    // Each cleanup row a body or a catch did not enter by ending holds the
    // exit its cleanup suspended, outermost first, with the value it took.
    let mut finally = Vec::new();
    record.finally.visit(access, &mut |_, value| {
        finally.push(value.clone());
        true
    })?;
    let mut values = departing.iter();
    let mut suspensions = Vec::new();
    for row in &finally {
        use crate::machine::Completion;
        use lash_kernel_state::paged::EnteredRecord;
        let mut value = || {
            values
                .next()
                .map(|value| Box::new(*value))
                .ok_or_else(malformed)
        };
        let completion = match &row.entered {
            EnteredRecord::Normal => continue,
            EnteredRecord::Break => Completion::Break,
            EnteredRecord::Continue => Completion::Continue,
            EnteredRecord::Return => Completion::Return(value()?),
            EnteredRecord::Throw => Completion::Throw(value()?),
            EnteredRecord::ReturnVia(_) => return Err(malformed()),
        };
        suspensions.push((row.site.resolve(access)?.as_ref().clone(), completion));
    }
    if values.next().is_some() || suspensions.len() != source.suspended.len() {
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
    let controls = control::controls_for(&code, pc, &loops, &source.roles, &suspensions)
        .map_err(|_| malformed())?;
    // The frame records each loop as the point stands in it: what it walks
    // and the item its header holds are its iterated rows.
    let mut rows = saved.iter();
    let mut iterated = 0;
    for control in controls.iter() {
        let (stmt, header, cursor, started) = match control {
            crate::machine::Control::For {
                stmt,
                cursor,
                started,
                header,
                guard,
            } => {
                iterated +=
                    u64::from(cursor.walked().is_some()) + u64::from(header.held().is_some());
                (
                    *stmt,
                    crate::machine::frames::header_record(header),
                    crate::machine::frames::cursor_record(cursor, guard.as_deref()),
                    *started,
                )
            }
            crate::machine::Control::While { stmt, started } => (
                *stmt,
                lash_kernel_state::paged::HeaderRecord::Body,
                None,
                *started,
            ),
            _ => continue,
        };
        let row = rows.next().ok_or_else(malformed)?;
        if row.site.resolve(access)?.as_ref() != &exe.stmt(stmt).site
            || row.header != header
            || row.cursor != cursor
            || row.started != started
        {
            return Err(malformed());
        }
    }
    if rows.next().is_some() || record.iterated.len() != iterated {
        return Err(malformed());
    }
    // Each cleanup the point stands in was entered by its body or its catch
    // block ending or holds the exit it suspended, in control order; no
    // other cleanup row is saved.
    let mut rows = finally.iter();
    for control in controls.iter() {
        let crate::machine::Control::Try {
            stmt,
            phase: crate::machine::TryPhase::Finally(completion),
        } = control
        else {
            continue;
        };
        let crate::compile::Form::Try { site, .. } = &exe.stmt(*stmt).form else {
            return Err(malformed());
        };
        let row = rows.next().ok_or_else(malformed)?;
        use crate::machine::Completion;
        use lash_kernel_state::paged::EnteredRecord;
        if !matches!(
            (&row.entered, completion),
            (EnteredRecord::Normal, Completion::Normal)
                | (EnteredRecord::Break, Completion::Break)
                | (EnteredRecord::Continue, Completion::Continue)
                | (EnteredRecord::Return, Completion::Return(_))
                | (EnteredRecord::Throw, Completion::Throw(_))
        ) || row.site.resolve(access)?.as_ref() != site
        {
            return Err(malformed());
        }
    }
    if rows.next().is_some() {
        return Err(malformed());
    }
    Ok((controls, standing(&code, source).map_err(|_| malformed())?))
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
            if let CheckedKind::Local { slot, .. } = kernel.kind()
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
