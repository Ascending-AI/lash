//! Static scalar representation selection. No candidate operation is evaluated.
use super::*;
use crate::compile::room::{CallSource, Owner};

fn merge(known: &mut Option<RegisterKind>, incoming: RegisterKind) -> bool {
    let next = match *known {
        None => incoming,
        Some(prior) if prior == incoming => prior,
        Some(_) => RegisterKind::Value,
    };
    if *known == Some(next) {
        false
    } else {
        *known = Some(next);
        true
    }
}

fn kinds(exe: &Executable, draft: &RegionDraft) -> Result<Box<[RegisterKind]>, Refused> {
    let mut known = vec![None; draft.registers.len()];
    known[draft.roles.len()] = Some(RegisterKind::Value);
    for (index, kind) in draft
        .registers
        .iter()
        .enumerate()
        .skip(draft.roles.len() + 1)
    {
        known[index] = Some(*kind);
    }
    for name in &draft.source.params {
        if let Ok(reg) = lower::reg(
            &draft.roles,
            &draft.source,
            &Owner::Binding {
                declared: draft.source.site.clone(),
                name: name.clone(),
            },
        ) {
            known[reg.offset()] = Some(RegisterKind::Value);
        }
    }
    for (slot, reg) in &draft.roles {
        if slot.capture() {
            known[reg.offset()] = Some(RegisterKind::Value);
        }
    }
    loop {
        let mut changed = false;
        for op in &draft.ops {
            let rule = match op {
                RegionOp::Constant { dst, value, .. } => Some((
                    *dst,
                    match value {
                        Value::Int(_) => Some(RegisterKind::Int),
                        Value::Bool(_) => Some(RegisterKind::Bool),
                        Value::Float(_) => Some(RegisterKind::Float),
                        _ => Some(RegisterKind::Value),
                    },
                )),
                RegionOp::Move { dst, src, .. } => Some((*dst, known[src.offset()])),
                RegionOp::Checked {
                    kernel, args, dst, ..
                } => match kernel.kind() {
                    CheckedKind::Local { slot } => {
                        let input = draft
                            .roles
                            .iter()
                            .find(|(candidate, _)| candidate == slot)
                            .and_then(|(_, reg)| known[reg.offset()]);
                        Some((*dst, input))
                    }
                    CheckedKind::FloatBinary { operation, .. } => Some((
                        *dst,
                        Some(if operation.compares() {
                            RegisterKind::Bool
                        } else {
                            RegisterKind::Float
                        }),
                    )),
                    CheckedKind::IntTruth { .. } => Some((*dst, Some(RegisterKind::Bool))),
                    CheckedKind::Native { lib, .. } => {
                        let types: Option<Vec<_>> =
                            args.iter().map(|reg| known[reg.offset()]).collect();
                        let result = types.map(|types| {
                            let function = exe.lib(*lib);
                            if types.len() == 2
                                && types.iter().all(|kind| *kind == RegisterKind::Int)
                                && let Some(op) =
                                    super::resource_pair(function, true).and_then(|pair| pair.ints)
                            {
                                if op.compares() {
                                    return RegisterKind::Bool;
                                }
                                if op != doc::scalar::ScalarOp::Div {
                                    return RegisterKind::Int;
                                }
                            }
                            if types.len() == 2
                                && types.iter().all(|kind| *kind == RegisterKind::Float)
                                && let Some(op) = super::resource_pair(function, true)
                                    .and_then(|pair| pair.floats)
                            {
                                return if op.compares() {
                                    RegisterKind::Bool
                                } else {
                                    RegisterKind::Float
                                };
                            }
                            match function.header.signature.result {
                                lash_kernel_value::Type::Int => RegisterKind::Int,
                                lash_kernel_value::Type::Bool => RegisterKind::Bool,
                                lash_kernel_value::Type::Float => RegisterKind::Float,
                                _ => RegisterKind::Value,
                            }
                        });
                        Some((*dst, result))
                    }
                    CheckedKind::Cursor { .. } => Some((*dst, Some(RegisterKind::Value))),
                    _ => None,
                },
                RegionOp::Heap { dst: Some(dst), .. } => Some((*dst, Some(RegisterKind::Value))),
                RegionOp::Read { dst, spec, .. } => Some((*dst, Some(spec.kind))),
                RegionOp::IntBinary { dst, .. } => Some((
                    Reg::<LeaseValue>::new(dst.offset())?,
                    Some(RegisterKind::Int),
                )),
                RegionOp::Call { .. } => {
                    Some((Reg::new(draft.roles.len())?, Some(RegisterKind::Value)))
                }
                _ => None,
            };
            if let Some((dst, Some(kind))) = rule {
                changed |= merge(&mut known[dst.offset()], kind);
            }
        }
        if !changed {
            break;
        }
    }
    Ok(known
        .into_iter()
        .map(|kind| kind.unwrap_or(RegisterKind::Value))
        .collect())
}

pub(super) fn specialize(exe: &Executable, draft: &mut RegionDraft) -> Result<(), Refused> {
    draft.registers = kinds(exe, draft)?;
    let mut preferred_reads = std::collections::BTreeMap::new();
    for op in &draft.ops {
        if let RegionOp::Checked {
            kernel:
                CheckedKernel {
                    kind: CheckedKind::Native { lib, .. },
                },
            args,
            ..
        } = op
            && let Some(pair) = super::resource_pair(exe.lib(*lib), true)
        {
            let floats = pair.floats.is_some()
                && (pair.ints.is_none()
                    || args
                        .iter()
                        .any(|reg| draft.registers[reg.offset()] == RegisterKind::Float));
            for reg in args {
                preferred_reads.insert(
                    reg.offset(),
                    if floats {
                        RegisterKind::Float
                    } else {
                        RegisterKind::Int
                    },
                );
            }
        }
    }
    for op in draft.ops.iter_mut() {
        if let RegionOp::Read { spec, dst, .. } = op
            && let Some(kind) = preferred_reads.get(&dst.offset())
        {
            spec.kind = *kind;
        }
    }
    draft.registers = kinds(exe, draft)?;
    for index in 0..draft.ops.len() {
        let replacement = if let RegionOp::Checked {
            kernel,
            args,
            dst,
            next,
        } = &draft.ops[index]
        {
            if let CheckedKind::Native { lib, function } = kernel.kind() {
                let native = exe.lib(*lib);
                let both = super::resource_pair(native, true);
                let floats = both.and_then(|pair| pair.floats).filter(|_| {
                    both.is_some_and(|pair| pair.ints.is_none())
                        || args
                            .iter()
                            .any(|reg| draft.registers[reg.offset()] == RegisterKind::Float)
                });
                let pair = both.and_then(|pair| pair.ints);
                let table = native
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(_, fast)| fast.scalar());
                if args.len() == 2
                    && let (Some(operation), Some(table)) = (floats, table)
                {
                    let mut numbers = [
                        Reg::<f64>::new(args[0].offset())?,
                        Reg::<f64>::new(args[1].offset())?,
                    ];
                    if args
                        .iter()
                        .any(|reg| draft.registers[reg.offset()] != RegisterKind::Float)
                    {
                        for (number, input) in numbers.iter_mut().zip(args.iter()) {
                            if draft.registers[input.offset()] != RegisterKind::Float {
                                *number = Reg::<f64>::new(draft.registers.len())?;
                                let mut kinds = std::mem::take(&mut draft.registers).into_vec();
                                kinds.push(RegisterKind::Float);
                                draft.registers = kinds.into();
                            }
                        }
                        draft.steps[index].guard = Some(CheckedKernel {
                            kind: CheckedKind::UnpackFloats {
                                inputs: [args[0], args[1]],
                                outputs: numbers,
                            },
                        });
                    }
                    Some(RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::FloatBinary {
                                function: *function,
                                lib: *lib,
                                operation,
                                lhs: numbers[0],
                                rhs: numbers[1],
                                units: table.units(0).ok_or(Refused::Facts)?,
                            },
                        },
                        args: args.clone(),
                        dst: *dst,
                        next: *next,
                    })
                } else if args.len() == 2
                    && matches!(
                        draft.source.calls.get(function),
                        Some(CallSource::Native(_))
                    )
                    && let (Some(operation), Some(table)) = (pair, table)
                {
                    let units = std::array::from_fn(|mask| table.units(mask as u8).unwrap_or(0));
                    let mut words = [
                        Reg::<i64>::new(args[0].offset())?,
                        Reg::<i64>::new(args[1].offset())?,
                    ];
                    if args
                        .iter()
                        .any(|reg| draft.registers[reg.offset()] != RegisterKind::Int)
                    {
                        for (word, input) in words.iter_mut().zip(args.iter()) {
                            if draft.registers[input.offset()] != RegisterKind::Int {
                                *word = Reg::<i64>::new(draft.registers.len())?;
                                let mut kinds = std::mem::take(&mut draft.registers).into_vec();
                                kinds.push(RegisterKind::Int);
                                draft.registers = kinds.into();
                            }
                        }
                        draft.steps[index].guard = Some(CheckedKernel {
                            kind: CheckedKind::UnpackPair {
                                inputs: [args[0], args[1]],
                                outputs: words,
                            },
                        });
                    }
                    let [lhs, rhs] = words;
                    if operation.compares() {
                        Some(RegionOp::Checked {
                            kernel: CheckedKernel {
                                kind: CheckedKind::IntTruth {
                                    function: *function,
                                    lib: *lib,
                                    operation,
                                    lhs,
                                    rhs,
                                    units,
                                },
                            },
                            args: args.clone(),
                            dst: *dst,
                            next: *next,
                        })
                    } else if operation != doc::scalar::ScalarOp::Div {
                        Some(RegionOp::IntBinary {
                            kernel: IntKernel {
                                function: *function,
                                lib: *lib,
                                op: operation,
                                units,
                                depth: draft.steps[index].depth,
                            },
                            lhs,
                            rhs,
                            dst: Reg::new(dst.offset())?,
                            next: *next,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(op) = replacement {
            draft.ops[index] = op;
        }
    }
    draft.registers = kinds(exe, draft)?;
    for index in 0..draft.ops.len() {
        // This optional scalar formula requires stable source retention.
        // Other lifetimes use actual lease graph facts at the same ISA prefix.
        draft.steps[index].scalar_resources = match &draft.ops[index] {
            RegionOp::Constant { .. } | RegionOp::IntBinary { .. } | RegionOp::Branch { .. } => {
                true
            }
            RegionOp::Move { dst, src, .. } => [dst, src].into_iter().all(|reg| {
                matches!(
                    draft.registers[reg.offset()],
                    RegisterKind::Int | RegisterKind::Float | RegisterKind::Bool
                )
            }),
            RegionOp::Checked { kernel, .. } => match kernel.kind() {
                CheckedKind::Local { .. }
                | CheckedKind::IntTruth { .. }
                | CheckedKind::FloatBinary { .. } => true,
                CheckedKind::Phase => draft.steps[index]
                    .peak
                    .terms
                    .iter()
                    .chain(draft.steps[index].delta.terms.iter())
                    .all(|(input, _)| match input {
                        ResourceInput::Local(reg) => matches!(
                            draft.registers[reg.offset()],
                            RegisterKind::Int | RegisterKind::Float | RegisterKind::Bool
                        ),
                        ResourceInput::Release(regs) => regs.iter().all(|reg| {
                            matches!(
                                draft.registers[reg.offset()],
                                RegisterKind::Int | RegisterKind::Float | RegisterKind::Bool
                            )
                        }),
                        ResourceInput::Result => false,
                        ResourceInput::SourceFact(_) => false,
                    }),
                _ => false,
            },
            RegionOp::Read { spec, .. } => spec.retained.is_some(),
            _ => false,
        } && draft.steps[index].event.entities.is_empty();
    }
    Ok(())
}
