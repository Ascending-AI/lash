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

/// The kind a native call's result has over arguments of `args` kinds: a
/// scalar pair's comparison or arithmetic over two ints or two floats, or
/// else what the signature promises. The kind inference and the verifier
/// both read it, so a register they type agrees.
pub(super) fn native_result(exe: &Executable, lib: LibId, args: &[RegisterKind]) -> RegisterKind {
    let function = exe.lib(lib);
    if args.len() == 2
        && args.iter().all(|kind| *kind == RegisterKind::Int)
        && let Some(op) = super::resource_pair(function, true).and_then(|pair| pair.ints)
    {
        if op.compares() {
            return RegisterKind::Bool;
        }
        if op != doc::scalar::ScalarOp::Div {
            return RegisterKind::Int;
        }
    }
    if args.len() == 2
        && args.iter().all(|kind| *kind == RegisterKind::Float)
        && let Some(op) = super::resource_pair(function, true).and_then(|pair| pair.floats)
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
    // A session cell holds whatever the lease imported or another frame
    // wrote, so its kind is never narrowed by this region's own writes.
    for (role, reg) in &draft.roles {
        if role.capture() || matches!(role, Role::Session(_)) {
            known[reg.offset()] = Some(RegisterKind::Value);
        }
    }
    loop {
        let mut changed = false;
        for (index, op) in draft.ops.iter().enumerate() {
            // Atom loads, delivered results, returns and departures also
            // define registers; every definition joins into the kind.
            let mut extra = Vec::new();
            let atoms = match op {
                RegionOp::Call { site, .. } => {
                    draft.calls.get(site.offset()).map(|call| &call.atoms[..])
                }
                RegionOp::Boundary { spec } => spec.action().map(|action| &action.atoms[..]),
                _ => None,
            };
            for load in atoms.into_iter().flatten() {
                extra.push((load.dst.offset(), Some(RegisterKind::Value)));
            }
            let output = draft.steps[index]
                .event
                .output
                .as_ref()
                .and_then(|owner| lower::reg(&draft.roles, &draft.source, owner).ok());
            match op {
                RegionOp::Boundary { spec } => {
                    if spec.action().is_some() {
                        extra.push((draft.roles.len(), Some(RegisterKind::Value)));
                    }
                    if let Some(output) = output {
                        extra.push((output.offset(), Some(RegisterKind::Value)));
                    }
                }
                RegionOp::Return { value } => {
                    if let Some(output) = output {
                        extra.push((output.offset(), known[value.offset()]));
                    }
                }
                RegionOp::Effect {
                    value,
                    departure: Some(departure),
                    ..
                } => extra.push((departure.offset(), known[value.offset()])),
                RegionOp::Raise { dst, src } | RegionOp::ReturnVia { dst, src } => {
                    extra.push((dst.offset(), known[src.offset()]))
                }
                _ => {}
            }
            for (dst, kind) in extra {
                if let Some(kind) = kind {
                    changed |= merge(&mut known[dst], kind);
                }
            }
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
                    CheckedKind::Local { slot, .. } => {
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
                        let result = types.map(|types| native_result(exe, *lib, &types));
                        Some((*dst, result))
                    }
                    CheckedKind::Cursor { .. }
                    | CheckedKind::Header { .. }
                    | CheckedKind::LoopItem { .. } => Some((*dst, Some(RegisterKind::Value))),
                    CheckedKind::Restore { exit, .. } | CheckedKind::Escape { exit, .. }
                        if super::departures::valued(*exit) =>
                    {
                        Some((*dst, Some(RegisterKind::Value)))
                    }
                    _ => None,
                },
                RegionOp::Heap { dst: Some(dst), .. } => Some((*dst, Some(RegisterKind::Value))),
                RegionOp::Read { dst, .. } => Some((*dst, Some(RegisterKind::Value))),
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
                // A kernel that unpacks unknown operands falls back to its
                // native when they are not its kind, so the native's every
                // answer must land in the kernel's result kind.
                let guarded = |kind: RegisterKind, operation: doc::scalar::ScalarOp| {
                    args.iter().all(|reg| draft.registers[reg.offset()] == kind)
                        || native.header.signature.result
                            == if operation.compares() {
                                lash_kernel_value::Type::Bool
                            } else if kind == RegisterKind::Int {
                                lash_kernel_value::Type::Int
                            } else {
                                lash_kernel_value::Type::Float
                            }
                };
                let floats = both
                    .and_then(|pair| pair.floats)
                    .filter(|_| {
                        both.is_some_and(|pair| pair.ints.is_none())
                            || args
                                .iter()
                                .any(|reg| draft.registers[reg.offset()] == RegisterKind::Float)
                    })
                    .filter(|operation| guarded(RegisterKind::Float, *operation));
                let pair = both
                    .and_then(|pair| pair.ints)
                    .filter(|operation| guarded(RegisterKind::Int, *operation));
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
