//! Definite typed definitions are checked over every reachable control edge.
use super::*;

pub(super) fn verify(exe: &Executable, draft: &RegionDraft) -> Result<(), Refused> {
    let mut initial = vec![None; draft.registers.len()];
    for name in &draft.source.params {
        if let Ok(reg) = register(
            draft,
            &Owner::Binding {
                declared: draft.source.site.clone(),
                name: name.clone(),
            },
        ) {
            initial[reg.offset()] = Some(RegisterKind::Value);
        }
    }
    // A capture enters with its frame; a session cell is the lease's and
    // holds whatever the lease imported or an earlier frame wrote.
    for (role, reg) in &draft.roles {
        if role.capture() || matches!(role, Role::Session(_)) {
            initial[reg.offset()] = Some(RegisterKind::Value);
        }
    }
    let mut entries = BTreeMap::from([(draft.entry.offset(), initial)]);
    let mut queue = VecDeque::from([draft.entry.offset()]);
    while let Some(index) = queue.pop_front() {
        let mut state = entries.get(&index).ok_or(Refused::Facts)?.clone();
        let before_state = state.clone();
        let step = &draft.steps[index];
        let read = |state: &[Option<RegisterKind>], reg: usize| {
            state.get(reg).copied().flatten().ok_or(Refused::Facts)
        };
        if let Some(guard) = &step.guard {
            match guard.kind() {
                CheckedKind::Truth { input, output } => {
                    read(&state, input.offset())?;
                    state[output.offset()] = Some(RegisterKind::Bool);
                }
                CheckedKind::UnpackFloats { inputs, outputs } => {
                    for (input, output) in inputs.iter().zip(outputs) {
                        read(&state, input.offset())?;
                        state[output.offset()] = Some(RegisterKind::Float);
                    }
                }
                CheckedKind::UnpackPair { inputs, outputs } => {
                    for (input, output) in inputs.iter().zip(outputs) {
                        read(&state, input.offset())?;
                        state[output.offset()] = Some(RegisterKind::Int);
                    }
                }
                _ => return Err(Refused::Facts),
            }
        }
        match &draft.ops[index] {
            RegionOp::Constant { dst, value, .. } => {
                state[dst.offset()] = Some(match value {
                    Value::Int(_) => RegisterKind::Int,
                    Value::Bool(_) => RegisterKind::Bool,
                    Value::Float(_) => RegisterKind::Float,
                    _ => RegisterKind::Value,
                })
            }
            RegionOp::Move { dst, src, .. } | RegionOp::Raise { dst, src } => {
                state[dst.offset()] = Some(read(&state, src.offset())?)
            }
            RegionOp::IntBinary { lhs, rhs, dst, .. } => {
                if read(&state, lhs.offset())? != RegisterKind::Int
                    || read(&state, rhs.offset())? != RegisterKind::Int
                {
                    return Err(Refused::Facts);
                }
                state[dst.offset()] = Some(RegisterKind::Int);
            }
            RegionOp::Read {
                base, dst, spec, ..
            } => {
                read(&state, base.offset())?;
                if let Selector::Index(key) = spec.selector() {
                    read(&state, key.offset())?;
                }
                state[dst.offset()] = Some(RegisterKind::Value);
                if step.scalar_resources && spec.retained.is_none() {
                    return Err(Refused::Retained);
                }
            }
            RegionOp::Checked {
                kernel, args, dst, ..
            } => match kernel.kind() {
                CheckedKind::Local { slot, .. } => {
                    let reg = draft
                        .roles
                        .iter()
                        .find(|(candidate, _)| candidate == slot)
                        .map(|(_, reg)| *reg)
                        .ok_or(Refused::Facts)?;
                    state[dst.offset()] = Some(read(&state, reg.offset())?);
                }
                CheckedKind::FloatBinary {
                    lhs,
                    rhs,
                    operation,
                    ..
                } => {
                    if read(&state, lhs.offset())? != RegisterKind::Float
                        || read(&state, rhs.offset())? != RegisterKind::Float
                    {
                        return Err(Refused::Facts);
                    }
                    state[dst.offset()] = Some(if operation.compares() {
                        RegisterKind::Bool
                    } else {
                        RegisterKind::Float
                    });
                }
                CheckedKind::IntTruth { lhs, rhs, .. } => {
                    if read(&state, lhs.offset())? != RegisterKind::Int
                        || read(&state, rhs.offset())? != RegisterKind::Int
                    {
                        return Err(Refused::Facts);
                    }
                    state[dst.offset()] = Some(RegisterKind::Bool);
                }
                CheckedKind::Native { lib, .. } => {
                    let kinds = args
                        .iter()
                        .map(|reg| read(&state, reg.offset()))
                        .collect::<Result<Vec<_>, _>>()?;
                    state[dst.offset()] =
                        Some(super::super::specialize::native_result(exe, *lib, &kinds));
                    if step.scalar_resources {
                        return Err(Refused::Retained);
                    }
                }
                CheckedKind::Cursor { .. } => {
                    for reg in args {
                        read(&state, reg.offset())?;
                    }
                    state[dst.offset()] = Some(RegisterKind::Value);
                }
                CheckedKind::LoopControl { iterable, .. } => {
                    if let Some(reg) = iterable {
                        read(&state, reg.offset())?;
                    }
                }
                CheckedKind::Phase => {}
                _ => return Err(Refused::Facts),
            },
            RegionOp::Branch { test, .. } => {
                if read(&state, test.offset())? != RegisterKind::Bool {
                    return Err(Refused::Facts);
                }
            }
            RegionOp::Call { site, .. } => {
                let call = draft.calls.get(site.offset()).ok_or(Refused::Source)?;
                for owner in &step.event.inputs {
                    read(&state, register(draft, owner)?.offset())?;
                }
                for load in &call.atoms {
                    if let AtomSource::Role(src) = load.src {
                        read(&state, src.offset())?;
                    }
                    state[load.dst.offset()] = Some(RegisterKind::Value);
                }
                for reg in &call.arguments {
                    read(&state, reg.offset())?;
                }
                state[draft.roles.len()] = Some(RegisterKind::Value)
            }
            RegionOp::Heap {
                args, dst, kernel, ..
            } => {
                if let HeapKind::Closure { captures, .. } = kernel.kind() {
                    for slot in captures {
                        let reg = draft
                            .roles
                            .iter()
                            .find_map(|(actual, reg)| {
                                (*actual == Role::Slot(*slot)).then_some(*reg)
                            })
                            .ok_or(Refused::Facts)?;
                        read(&state, reg.offset())?;
                    }
                }
                if step.scalar_resources {
                    return Err(Refused::Retained);
                }
                for reg in args {
                    read(&state, reg.offset())?;
                }
                if let Some(dst) = dst {
                    state[dst.offset()] = Some(RegisterKind::Value);
                }
            }
            RegionOp::Boundary { spec } => {
                // A delivering action loads its atoms; its result arrives in
                // the sink before its Commit.
                if let Some(action) = spec.action() {
                    for load in &action.atoms {
                        if let AtomSource::Role(src) = load.src {
                            read(&state, src.offset())?;
                        }
                        state[load.dst.offset()] = Some(RegisterKind::Value);
                    }
                    for reg in &action.arguments {
                        read(&state, reg.offset())?;
                    }
                    state[draft.roles.len()] = Some(RegisterKind::Value);
                }
                if let Some(output) = step
                    .event
                    .output
                    .as_ref()
                    .and_then(|owner| register(draft, owner).ok())
                {
                    state[output.offset()] = Some(RegisterKind::Value);
                }
            }
            RegionOp::Return { value } => {
                let kind = read(&state, value.offset())?;
                let output = step.event.output.as_ref().ok_or(Refused::Facts)?;
                state[register(draft, output)?.offset()] = Some(kind);
            }
            RegionOp::ReturnVia { value } => {
                read(&state, value.offset())?;
            }
            RegionOp::Effect {
                value, departure, ..
            } => {
                let kind = read(&state, value.offset())?;
                if let Some(departure) = departure {
                    state[departure.offset()] = Some(kind);
                }
            }
            RegionOp::Exit { .. } => {}
        }
        if step.scalar_resources
            && matches!(
                draft.ops[index],
                RegionOp::Move { .. }
                    | RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::Phase
                        },
                        ..
                    }
            )
        {
            for (fact, _) in step.peak.terms.iter().chain(step.delta.terms.iter()) {
                if let ResourceInput::Release(regs) = fact {
                    for reg in regs {
                        if !step
                            .event
                            .inputs
                            .iter()
                            .any(|owner| register(draft, owner) == Ok(*reg))
                            && !matches!(
                                before_state[reg.offset()],
                                Some(RegisterKind::Int | RegisterKind::Float | RegisterKind::Bool)
                                    | None
                            )
                        {
                            return Err(Refused::Retained);
                        }
                    }
                }
            }
        }
        for successor in recovery::successors(draft, index) {
            let next = entries.get_mut(&successor.offset());
            match next {
                None => {
                    entries.insert(successor.offset(), state.clone());
                    queue.push_back(successor.offset());
                }
                Some(prior) => {
                    let mut changed = false;
                    for (old, new) in prior.iter_mut().zip(&state) {
                        let joined = match (*old, *new) {
                            (Some(a), Some(b)) if a == b => Some(a),
                            (Some(_), Some(_)) => Some(RegisterKind::Value),
                            _ => None,
                        };
                        if *old != joined {
                            *old = joined;
                            changed = true;
                        }
                    }
                    if changed {
                        queue.push_back(successor.offset());
                    }
                }
            }
        }
    }
    Ok(())
}
