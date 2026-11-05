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
    for (slot, reg) in &draft.roles {
        if matches!(slot, Slot::Capture(_)) {
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
            RegionOp::Move { dst, src, .. } => {
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
                state[dst.offset()] = Some(spec.kind);
                if step.scalar_resources && spec.retained.is_none() {
                    return Err(Refused::Retained);
                }
            }
            RegionOp::Checked {
                kernel, args, dst, ..
            } => match kernel.kind() {
                CheckedKind::Local { slot } => {
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
                    for reg in args {
                        read(&state, reg.offset())?;
                    }
                    state[dst.offset()] = Some(match exe.lib(*lib).header.signature.result {
                        lash_kernel_value::Type::Bool => RegisterKind::Bool,
                        lash_kernel_value::Type::Int => RegisterKind::Int,
                        _ => RegisterKind::Value,
                    });
                    if step.scalar_resources {
                        return Err(Refused::Retained);
                    }
                }
                CheckedKind::Cursor { .. } => state[dst.offset()] = Some(RegisterKind::Value),
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
                let _ = draft.calls.get(site.offset()).ok_or(Refused::Source)?;
                state[draft.roles.len()] = Some(RegisterKind::Value)
            }
            RegionOp::Heap { args, dst, .. } => {
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
            RegionOp::Boundary { .. } => {
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
